// @bedrock-rs/core is built into the BedrockRS server, which gives it to the
// plugins it runs: every function in it is native code in the server. This
// package only carries its types (index.d.ts), for editors and TypeScript, so
// importing it anywhere else is an error.
throw new Error(
  "@bedrock-rs/core is provided by the BedrockRS server: it can only be imported " +
    "by a plugin running there",
);
