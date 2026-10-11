// Text files imported with `with { type: "text" }`; Bun embeds them in the compiled installer.
declare module "*.asc" {
  const text: string;
  export default text;
}
