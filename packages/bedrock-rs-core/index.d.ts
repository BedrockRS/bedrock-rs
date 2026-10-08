// Types for @bedrock-rs/core, the BedrockRS plugin API.
//
// These are for your editor and the TypeScript checker. At runtime the
// module comes from the server, which implements it natively.

/** Writes to the server's console, tagged with the plugin's name. `console` writes here too. */
export declare const Logger: {
  readonly trace: (...args: unknown[]) => void;
  readonly debug: (...args: unknown[]) => void;
  readonly info: (...args: unknown[]) => void;
  readonly warn: (...args: unknown[]) => void;
  readonly error: (...args: unknown[]) => void;
};

/** A player who is (or was) online. Acting on a player who has left does nothing. */
export declare class Player {
  private constructor();
  /** The name shown in game. Players can change it: key saved data by `uuid`. */
  readonly name: string;
  /** The player's persistent identity, the same across sessions and name changes. */
  readonly uuid: string;
  /** Shows a message in the player's chat. */
  sendMessage(message: string): void;
  /** Disconnects the player, showing them `reason`. */
  kick(reason?: string): void;
  setGameMode(mode: GameMode): void;
  /** Sets the player's health; 0 kills them. */
  setHealth(health: number): void;
  /** Hurts the player, as `cause` would (default "none"). */
  damage(amount: number, cause?: DamageCause): void;
}

export type GameMode =
  | "survival"
  | "creative"
  | "adventure"
  | "spectator"
  | "s"
  | "c"
  | "a"
  | "default"
  | "d";

export type DamageCause =
  | "anvil" | "blockExplosion" | "campfire" | "contact" | "drowning" | "entityAttack"
  | "entityExplosion" | "fall" | "fallingBlock" | "fire" | "fireTick" | "fireworks"
  | "flyIntoWall" | "freezing" | "lava" | "lightning" | "maceSmash" | "magic" | "magma"
  | "none" | "override" | "piston" | "projectile" | "ramAttack" | "selfDestruct"
  | "sonicBoom" | "soulCampfire" | "stalactite" | "stalagmite" | "starve" | "suffocation"
  | "temperature" | "thorns" | "void" | "wither";

/** The world, and its scheduler. Ticks are 1/20 of a second; ids are what clearRun takes. */
export declare class World {
  private constructor();
  /** Runs `callback` on the next tick. */
  run(callback: () => unknown): number;
  /** Runs `callback` once, after `ticks` ticks. */
  runTimeout(callback: () => unknown, ticks?: number): number;
  /** Runs `callback` every `ticks` ticks. */
  runInterval(callback: () => unknown, ticks?: number): number;
  /** Stops a task started with run, runTimeout or runInterval. */
  clearRun(id: number): void;
  /** Resolves after `ticks` ticks: `await world.waitTicks(20)`. */
  waitTicks(ticks?: number): Promise<void>;
}

/** Something that can be stopped from happening. */
export interface Cancellable {
  cancel(): void;
  isCancelled(): boolean;
}

export interface BlockPosition {
  readonly x: number;
  readonly y: number;
  readonly z: number;
}

/** What each event's handlers receive. */
export interface Events {
  /** A player finished loading; cancelling stops vanilla's "joined the game" message. */
  player_join: { readonly player: Player } & Cancellable;
  /** A player left; cancelling stops vanilla's "left the game" message. */
  player_quit: { readonly player: Player } & Cancellable;
  /** A player sent a chat message; cancelling means nobody sees it. */
  player_chat: { readonly player: Player; readonly message: string } & Cancellable;
  block_break: { readonly player: Player; readonly position: BlockPosition; readonly block: string };
  block_place: { readonly player: Player; readonly position: BlockPosition; readonly block: string };
  /** A player is about to take damage; cancelling prevents it. */
  player_damage: {
    readonly player: Player;
    readonly cause: DamageCause;
    readonly amount: number;
    /** Health before the damage. */
    readonly health: number;
  } & Cancellable;
  /** A player died; `message` is the death message in English. */
  player_death: { readonly player: Player; readonly cause: DamageCause; readonly message: string };
  player_respawn: { readonly player: Player };
}

export type ArgumentType =
  | "string"
  | "text"
  | "int"
  | "number"
  | "bool"
  | "player"
  | "gamemode"
  | "enum";

export interface ArgumentDefinition {
  name: string;
  /** Default "string". `text` takes the rest of the line and must come last. */
  type?: ArgumentType;
  /** Optional arguments come after the required ones. */
  optional?: boolean;
  /** For `enum`: the values accepted. */
  values?: string[];
  /** For `enum`: the type name shown, e.g. <mode: Mode>. */
  enum?: string;
}

export interface CommandContext {
  /** Who ran the command, or undefined for the console. */
  readonly sender: Player | undefined;
  readonly console: boolean;
  /** The command's name, whichever alias was typed. */
  readonly command: string;
  /** The subcommands typed. */
  readonly path: readonly string[];
  /** The arguments by name; optional ones left out are undefined. */
  readonly args: Readonly<Record<string, string | number | boolean | Player | undefined>>;
  /** Replies to whoever ran the command. */
  reply(message: string): void;
  /** Replies in red. */
  error(message: string): void;
}

export interface SubcommandDefinition {
  description?: string;
  /** "any" (the default) or "operator"; subcommands can only narrow it. */
  permission?: "any" | "operator";
  args?: ArgumentDefinition[];
  /** Runs the command. A string returned is replied. */
  run?: (ctx: CommandContext) => unknown;
  subcommands?: Record<string, SubcommandDefinition>;
}

export interface CommandDefinition extends SubcommandDefinition {
  /** Lower case letters, digits, `_` and `-`. */
  name: string;
  aliases?: string[];
}

/** The server: players, broadcasting, events, commands, and the world. */
export declare const Server: {
  /** The world, whose scheduler runs code over time. */
  readonly getWorld: () => World;
  /** Shows a message in every player's chat. */
  readonly broadcast: (message: string) => void;
  /** The online player with this UUID. */
  readonly getPlayer: (uuid: string) => Player | undefined;
  /** Everyone online, by name. */
  readonly getPlayers: () => Player[];
  /** Calls `handler` for every `event`. From on_load on. */
  readonly on: <E extends keyof Events>(event: E, handler: (event: Events[E]) => unknown) => void;
  /** Adds a slash command. Only in on_load. */
  readonly registerCommand: (definition: CommandDefinition) => void;
};
