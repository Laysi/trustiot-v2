/** The roles that may change machines and production records (design.md, 「角色」): editors and administrators. */
export function canEditMachines(role: string | undefined): boolean {
  return role === "editor" || role === "admin";
}
