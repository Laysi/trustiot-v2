/**
 * Test fixture: the smallest 64-bit little-endian ELF file that the binary checks in lib.ts
 * read (aarch64ElfProblem, staticElfProblem). Only tests import it.
 */

/** `e_machine` values: aarch64 and x86_64. */
export const ELF_AARCH64 = 183;
export const ELF_X86_64 = 62;
/** `p_type` values: a loadable segment, the dynamic section, the program interpreter. */
export const PT_LOAD = 1;
export const PT_DYNAMIC = 2;
export const PT_INTERP = 3;

const HEADER_SIZE = 64;
const PROGRAM_HEADER_SIZE = 56;

/** An ELF header for `machine`, followed by one program header per entry of `segments`. */
export function elfFile(machine: number, segments: readonly number[], elfClass = 2): Uint8Array {
  const bytes = new Uint8Array(HEADER_SIZE + PROGRAM_HEADER_SIZE * segments.length);
  const view = new DataView(bytes.buffer);
  bytes.set([0x7f, 0x45, 0x4c, 0x46, elfClass, 1, 1], 0);
  view.setUint16(16, 3, true); // ET_DYN, as a static PIE is
  view.setUint16(18, machine, true);
  view.setBigUint64(32, BigInt(HEADER_SIZE), true);
  view.setUint16(54, PROGRAM_HEADER_SIZE, true);
  view.setUint16(56, segments.length, true);
  segments.forEach((type, index) => view.setUint32(HEADER_SIZE + index * PROGRAM_HEADER_SIZE, type, true));
  return bytes;
}

/** What Rust builds for aarch64-unknown-linux-musl: a static PIE, with a dynamic section and no interpreter. */
export function staticAarch64Binary(): Uint8Array {
  return elfFile(ELF_AARCH64, [PT_LOAD, PT_LOAD, PT_DYNAMIC]);
}

/** A binary that needs a dynamic loader, as an aarch64-unknown-linux-gnu build does. */
export function dynamicAarch64Binary(): Uint8Array {
  return elfFile(ELF_AARCH64, [PT_INTERP, PT_LOAD, PT_DYNAMIC]);
}
