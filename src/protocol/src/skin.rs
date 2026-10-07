//! The skin a client sends in its Login's client data JWT.
//!
//! Every field is checked before the skin goes to other players, since a
//! malformed skin could crash their clients: image sizes must match their
//! pixel data, the geometry and resource patch must be JSON, and sizes are
//! bounded. Character-creator pieces, their tints and the skin colour are
//! passed on as sent: clients fall back to a default skin for a persona skin
//! without them (found live on 2026-09-27).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use uuid::Uuid;

use crate::login::{Jwt, LoginError};
use crate::packets::{Cape, PersonaPiece, PieceTint, Skin, SkinAnimation, persona_piece_type};

/// Largest side of a skin, cape or animation image, in pixels.
const MAX_IMAGE_SIDE: u32 = 512;
/// Largest geometry JSON accepted; character-creator skins send a few
/// hundred kilobytes.
const MAX_GEOMETRY: usize = 4 * 1024 * 1024;
const MAX_ANIMATIONS: usize = 16;
const MAX_PIECES: usize = 64;

/// Why a client's skin cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum SkinError {
    #[error(transparent)]
    Login(#[from] LoginError),
    #[error("invalid client data: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} is not valid base64")]
    Base64(&'static str),
    #[error("{what} is {width}×{height} but has {len} bytes")]
    ImageSize {
        what: &'static str,
        width: u32,
        height: u32,
        len: usize,
    },
    #[error("{0} is not JSON")]
    NotJson(&'static str),
    #[error("{0} is too large")]
    TooLarge(&'static str),
}

/// The skin fields of the client data claims.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientSkin {
    #[serde(default)]
    skin_id: String,
    #[serde(default, rename = "PlayFabId")]
    play_fab_id: String,
    skin_resource_patch: String,
    skin_image_width: u32,
    skin_image_height: u32,
    skin_data: String,
    #[serde(default)]
    animated_image_data: Vec<ClientAnimation>,
    #[serde(default)]
    cape_image_width: u32,
    #[serde(default)]
    cape_image_height: u32,
    #[serde(default)]
    cape_data: String,
    #[serde(default)]
    cape_on_classic_skin: bool,
    #[serde(rename = "SkinGeometryData")]
    skin_geometry: String,
    #[serde(default, rename = "SkinGeometryDataEngineVersion")]
    skin_geometry_version: String,
    #[serde(default)]
    skin_animation_data: String,
    #[serde(default)]
    arm_size: String,
    #[serde(default)]
    persona_skin: bool,
    #[serde(default)]
    premium_skin: bool,
    #[serde(default, rename = "SkinColor")]
    skin_colour: String,
    #[serde(default)]
    persona_pieces: Vec<ClientPiece>,
    #[serde(default, rename = "PieceTintColors")]
    piece_tints: Vec<ClientTint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientPiece {
    #[serde(default)]
    piece_id: String,
    #[serde(default)]
    piece_type: String,
    #[serde(default)]
    pack_id: String,
    #[serde(default)]
    is_default: bool,
    #[serde(default)]
    product_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientTint {
    #[serde(default)]
    piece_type: String,
    #[serde(default)]
    colors: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientAnimation {
    image: String,
    image_width: u32,
    image_height: u32,
    frames: f32,
    #[serde(rename = "Type")]
    kind: u32,
    #[serde(default)]
    animation_expression: u32,
}

/// The skin in a client data JWT, checked. Its signature is not checked:
/// the connection already belongs to the client that sent it.
pub fn client_skin(client_data: &str) -> Result<Skin, SkinError> {
    let claims = Jwt::parse(client_data)?.claims;
    let client: ClientSkin = serde_json::from_value(claims)?;

    let data = decode("SkinData", &client.skin_data)?;
    check_image(
        "the skin",
        client.skin_image_width,
        client.skin_image_height,
        &data,
    )?;

    let cape = if client.cape_data.is_empty() {
        None
    } else {
        let data = decode("CapeData", &client.cape_data)?;
        check_image(
            "the cape",
            client.cape_image_width,
            client.cape_image_height,
            &data,
        )?;
        Some(Cape {
            // A fresh ID, as Dragonfly sends, so clients do not mix up capes.
            id: Uuid::new_v4().to_string(),
            width: client.cape_image_width,
            height: client.cape_image_height,
            data,
        })
    };

    if client.animated_image_data.len() > MAX_ANIMATIONS {
        return Err(SkinError::TooLarge("the animation list"));
    }
    let animations = client
        .animated_image_data
        .iter()
        .map(|animation| {
            let data = decode("AnimatedImageData", &animation.image)?;
            check_image(
                "an animation",
                animation.image_width,
                animation.image_height,
                &data,
            )?;
            Ok(SkinAnimation {
                width: animation.image_width,
                height: animation.image_height,
                data,
                kind: animation.kind,
                frames: animation.frames,
                expression: animation.animation_expression,
            })
        })
        .collect::<Result<_, SkinError>>()?;

    let resource_patch = decode_text("SkinResourcePatch", &client.skin_resource_patch)?;
    check_json("the resource patch", &resource_patch)?;
    let geometry = decode_text("SkinGeometryData", &client.skin_geometry)?;
    if geometry.len() > MAX_GEOMETRY {
        return Err(SkinError::TooLarge("the geometry"));
    }
    check_json("the geometry", &geometry)?;
    let geometry_engine_version = match decode_text(
        "SkinGeometryDataEngineVersion",
        &client.skin_geometry_version,
    )? {
        version if version.is_empty() => "0.0.0".to_owned(),
        version => version,
    };
    let animation_data = decode_text("SkinAnimationData", &client.skin_animation_data)?;

    if client.persona_pieces.len() > MAX_PIECES || client.piece_tints.len() > MAX_PIECES {
        return Err(SkinError::TooLarge("the persona pieces"));
    }
    let persona_pieces = client
        .persona_pieces
        .into_iter()
        .map(|piece| PersonaPiece {
            kind: persona_piece_type::from_login(&piece.piece_type),
            pack_id: Uuid::parse_str(&piece.pack_id).unwrap_or_default(),
            id: piece.piece_id,
            default: piece.is_default,
            product_id: piece.product_id,
        })
        .collect();
    let piece_tints = client
        .piece_tints
        .iter()
        .map(|tint| {
            let mut colours = [[0; 4]; 4];
            for (colour, text) in colours.iter_mut().zip(&tint.colors) {
                *colour = colour_from_hex(text);
            }
            PieceTint {
                piece_type: PieceTint::wire_type(&tint.piece_type),
                colours,
            }
        })
        .collect();

    Ok(Skin {
        // A fresh ID, as Dragonfly sends; the client's own goes in the full ID.
        id: Uuid::new_v4().to_string(),
        full_id: if client.skin_id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            client.skin_id
        },
        play_fab_id: client.play_fab_id,
        resource_patch,
        width: client.skin_image_width,
        height: client.skin_image_height,
        data,
        animations,
        cape,
        geometry,
        geometry_engine_version,
        animation_data,
        wide_arms: !client.arm_size.eq_ignore_ascii_case("slim"),
        colour: colour_from_hex(&client.skin_colour),
        persona_pieces,
        piece_tints,
        persona: client.persona_skin,
        premium: client.premium_skin,
        persona_cape_on_classic: client.cape_on_classic_skin,
    })
}

/// An RGBA colour from a login's hex notation: `#RRGGBB` (opaque) or
/// `#AARRGGBB`; anything shorter, such as `#0`, is ARGB with leading zeros,
/// and anything unreadable is transparent black.
fn colour_from_hex(text: &str) -> [u8; 4] {
    let digits = text.trim_start_matches('#');
    let Ok(mut value) = u32::from_str_radix(digits, 16) else {
        return [0; 4];
    };
    if digits.len() == 6 {
        value |= 0xFF00_0000;
    }
    let [alpha, red, green, blue] = value.to_be_bytes();
    [red, green, blue, alpha]
}

fn decode(field: &'static str, value: &str) -> Result<Vec<u8>, SkinError> {
    STANDARD.decode(value).map_err(|_| SkinError::Base64(field))
}

fn decode_text(field: &'static str, value: &str) -> Result<String, SkinError> {
    String::from_utf8(decode(field, value)?).map_err(|_| SkinError::NotJson(field))
}

fn check_image(what: &'static str, width: u32, height: u32, data: &[u8]) -> Result<(), SkinError> {
    let fits = width > 0 && height > 0 && width <= MAX_IMAGE_SIDE && height <= MAX_IMAGE_SIDE;
    if !fits || (width * height * 4) as usize != data.len() {
        return Err(SkinError::ImageSize {
            what,
            width,
            height,
            len: data.len(),
        });
    }
    Ok(())
}

fn check_json(what: &'static str, text: &str) -> Result<(), SkinError> {
    serde_json::from_str::<serde::de::IgnoredAny>(text)
        .map(|_| ())
        .map_err(|_| SkinError::NotJson(what))
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};

    use super::*;

    fn jwt(claims: Value) -> String {
        format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"ES384"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn b64(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    fn client_data() -> Value {
        json!({
            "SkinId": "c18e65aa-7b21-4637-9b63-8ad63622ef01.Custom",
            "PlayFabId": "abc123",
            "SkinResourcePatch": b64(br#"{"geometry":{"default":"geometry.humanoid.customSlim"}}"#),
            "SkinImageWidth": 64,
            "SkinImageHeight": 64,
            "SkinData": b64(&[200; 64 * 64 * 4]),
            "AnimatedImageData": [{
                "Image": b64(&[1; 32 * 64 * 4]),
                "ImageWidth": 32,
                "ImageHeight": 64,
                "Frames": 2.0,
                "Type": 1,
                "AnimationExpression": 1
            }],
            "CapeImageWidth": 64,
            "CapeImageHeight": 32,
            "CapeData": b64(&[3; 64 * 32 * 4]),
            "CapeId": "some-cape",
            "CapeOnClassicSkin": false,
            "SkinGeometryData": b64(br#"{"format_version":"1.12.0","minecraft:geometry":[]}"#),
            "SkinGeometryDataEngineVersion": b64(b"1.26.51"),
            "SkinAnimationData": "",
            "ArmSize": "slim",
            "PersonaSkin": true,
            "PremiumSkin": true,
            "SkinColor": "#b37b62",
            "PersonaPieces": [
                {
                    "IsDefault": true,
                    "PackId": "c0c4a5e9-2c21-4a8e-9f8f-6f8e4f1d2c3b",
                    "PieceId": "8f96d1f8-e9bb-40d2-acc8-eb79746c5d7c",
                    "PieceType": "persona_eyes",
                    "ProductId": ""
                },
                {
                    "IsDefault": false,
                    "PackId": "not a uuid",
                    "PieceId": "hair-piece",
                    "PieceType": "persona_hair",
                    "ProductId": "product"
                }
            ],
            "PieceTintColors": [
                {
                    "Colors": ["#ffa12722", "#ff2f1f0f", "#ff3aafd9", "#0"],
                    "PieceType": "persona_eyes"
                }
            ],
            "DeviceOS": 7
        })
    }

    #[test]
    fn reads_the_whole_skin() {
        let skin = client_skin(&jwt(client_data())).unwrap();
        assert_eq!(
            (skin.width, skin.height, skin.data.len()),
            (64, 64, 64 * 64 * 4)
        );
        assert_eq!(skin.full_id, "c18e65aa-7b21-4637-9b63-8ad63622ef01.Custom");
        assert_ne!(skin.id, skin.full_id, "a fresh ID");
        assert!(skin.resource_patch.contains("customSlim"));
        assert!(skin.geometry.starts_with('{'));
        assert_eq!(skin.geometry_engine_version, "1.26.51");
        assert!(!skin.wide_arms && skin.persona);
        let cape = skin.cape.unwrap();
        assert_eq!((cape.width, cape.height), (64, 32));
        assert_eq!(skin.animations.len(), 1);
        assert_eq!(skin.animations[0].frames, 2.0);
    }

    #[test]
    fn persona_pieces_tints_and_flags_are_kept() {
        let skin = client_skin(&jwt(client_data())).unwrap();
        assert!(skin.persona && skin.premium);
        assert_eq!(skin.colour, [0xB3, 0x7B, 0x62, 0xFF]);
        assert_eq!(skin.persona_pieces.len(), 2);
        let eyes = &skin.persona_pieces[0];
        assert_eq!((eyes.kind, eyes.default), (13, true));
        assert_eq!(
            eyes.pack_id.to_string(),
            "c0c4a5e9-2c21-4a8e-9f8f-6f8e4f1d2c3b"
        );
        let hair = &skin.persona_pieces[1];
        assert_eq!(
            (hair.kind, hair.id.as_str(), hair.product_id.as_str()),
            (14, "hair-piece", "product")
        );
        assert!(hair.pack_id.is_nil(), "an unreadable pack ID");
        let tint = &skin.piece_tints[0];
        assert_eq!(tint.piece_type, "eyes");
        assert_eq!(
            tint.colours,
            [
                [0xA1, 0x27, 0x22, 0xFF],
                [0x2F, 0x1F, 0x0F, 0xFF],
                [0x3A, 0xAF, 0xD9, 0xFF],
                [0, 0, 0, 0]
            ]
        );
    }

    #[test]
    fn capes_and_versions_are_optional() {
        let mut data = client_data();
        data["CapeData"] = json!("");
        data["SkinGeometryDataEngineVersion"] = json!("");
        data["ArmSize"] = json!("wide");
        let skin = client_skin(&jwt(data)).unwrap();
        assert!(skin.cape.is_none());
        assert_eq!(skin.geometry_engine_version, "0.0.0");
        assert!(skin.wide_arms);
    }

    #[test]
    fn broken_skins_are_refused() {
        let broken: [(&str, Value); 6] = [
            ("SkinData", json!(b64(&[1; 100]))),
            ("SkinImageWidth", json!(4096)),
            ("SkinGeometryData", json!(b64(b"not json"))),
            ("SkinResourcePatch", json!("%%% not base64")),
            ("CapeData", json!(b64(&[1; 10]))),
            (
                "AnimatedImageData",
                json!([{ "Image": b64(&[1; 8]), "ImageWidth": 32, "ImageHeight": 64, "Frames": 1.0, "Type": 1 }]),
            ),
        ];
        for (field, value) in broken {
            let mut data = client_data();
            data[field] = value;
            assert!(client_skin(&jwt(data)).is_err(), "{field}");
        }
        assert!(client_skin("not a jwt").is_err());
    }
}
