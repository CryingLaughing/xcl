import { createHash } from 'node:crypto';
import { mkdirSync, readdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { deflateSync } from 'node:zlib';

// Container and control-flow shape from gpu_compute_e2e::split_program on
// upstream 8b957bec6f82495aec940944c6bf47faa02db590. Opcode layouts match rdna2_spec.rs.
const UPSTREAM = '8b957bec6f82495aec940944c6bf47faa02db590';
const SHADER_COUNT = 160;
const END_PROGRAM = 0xbf810000;

function dwords(...words) {
  const bytes = Buffer.alloc(words.length * 4);
  words.forEach((word, index) => bytes.writeUInt32LE(word >>> 0, index * 4));
  return bytes;
}

function vectorMove(destination, source) {
  return 0x7e000000 | (destination << 17) | (1 << 9) | source;
}

function vectorOp(opcode, destination, source, vectorSource) {
  return (opcode << 25) | (destination << 17) | (vectorSource << 9) | source;
}

function guardedBlock(scalar, compare, salt, firstAdd) {
  const body = [
    vectorOp(37, 2, 128 + firstAdd, 2),
    vectorOp(29, 2, 255, 2), salt,
    vectorOp(37, 2, 130 + firstAdd, 2),
    vectorOp(29, 2, 132 + firstAdd, 2),
    vectorOp(37, 2, 134 + firstAdd, 2),
    vectorOp(29, 2, 136 + firstAdd, 2),
  ];
  const compareEqual = 0xbf060000 | (255 << 8) | scalar;
  const branchIfEqual = 0xbf850000 | body.length;
  return [compareEqual, compare, branchIfEqual, ...body];
}

function program(variant) {
  const setExecLo = 0xbe800000 | (126 << 16) | (3 << 8) | 126;
  const store = [0xdc008000 | (28 << 18), 4 | (2 << 8) | (0x7d << 16)];
  // The conditions and the three live XOR literals vary in every program.
  // This keeps all five basic blocks distinct after translation, rather than
  // changing only an outer container hash or using unused instructions.
  return dwords(
    setExecLo, vectorMove(2, 256),
    ...guardedBlock(1, variant, 0x10000000 + variant * 257, 1),
    ...guardedBlock(2, variant + 191, 0x20000000 + variant * 509, 13),
    vectorOp(26, 4, 130, 0), vectorOp(37, 4, 0, 4), vectorMove(5, 128),
    vectorOp(29, 2, 255, 2), 0x30000000 + variant * 1021,
    ...store, END_PROGRAM,
  );
}

function consoleComputeShader(code) {
  const codeOffset = 0x308;
  const fssm = Buffer.alloc(codeOffset + code.length);
  fssm.write('FSSM', 0);
  fssm.writeUInt32LE(0x101, 4);
  fssm.writeUInt32LE(fssm.length - 0x10, 8);
  fssm.writeBigUInt64LE(BigInt(codeOffset - 0x10), 0xc0);
  fssm.writeBigUInt64LE(BigInt(code.length), 0xc8);
  fssm.writeUInt32LE(1 << 5, 0xd4); // wave32
  fssm.writeBigUInt64LE(0x000000c000020106n, 0x100); // compute API stage descriptor
  dwords((2 << 1) | (1 << 7), 32, 1, 1).copy(fssm, 0x138);
  code.copy(fssm, codeOffset);

  const compressed = deflateSync(fssm, { level: 6 });
  const xbch = Buffer.concat([Buffer.from('XBCH'), dwords(3, compressed.length, 0, fssm.length, 0), compressed]);
  const table = 0x18 + 12;
  const data = table + 2 * 12;
  const dxilBody = Buffer.concat([
    dwords((5 << 16) | 0x66, 0), Buffer.from('DXIL'), dwords(0x106, 0, 4),
    Buffer.from('XDAT'), dwords(2, table, 0x12, data, 16, 0x0d, data + 16, xbch.length),
    Buffer.alloc(16, 0x5a), xbch,
  ]);
  const nobc = (dxilBody.length + 3) & ~3;
  const dxil = Buffer.alloc(nobc + 4);
  dxilBody.copy(dxil);
  dxil.writeUInt32LE(dxil.length / 4, 4);
  dxil.writeUInt32LE(nobc - 8, 16);
  dxil.write('NOBC', nobc);

  const dxbc = Buffer.alloc(36);
  dxbc.write('DXBC', 0);
  dxbc.writeUInt32LE(1, 20);
  dxbc.writeUInt32LE(dxbc.length + 8 + dxil.length, 24);
  dxbc.writeUInt32LE(1, 28);
  dxbc.writeUInt32LE(dxbc.length, 32);
  return Buffer.concat([dxbc, Buffer.from('DXIL'), dwords(dxil.length), dxil]);
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

const output = process.argv[2];
if (!output) throw new Error('Usage: node generate-split-gate-corpus.mjs <output-directory>');
const root = resolve(output);
const corpus = join(root, 'corpus');
mkdirSync(corpus, { recursive: true });
const filenames = Array.from({ length: SHADER_COUNT }, (_, i) => `split-${String(i + 1).padStart(3, '0')}.bin`);
const expectedNames = new Set(filenames);
const unexpected = readdirSync(corpus).filter((name) => !expectedNames.has(name));
if (unexpected.length) throw new Error(`Unexpected corpus entries: ${unexpected.join(', ')}`);
const files = filenames.map((name, index) => {
  const blob = consoleComputeShader(program(index + 1));
  writeFileSync(join(corpus, name), blob);
  return { name, bytes: blob.length, sha256: sha256(blob) };
});
if (new Set(files.map((file) => file.sha256)).size !== SHADER_COUNT) throw new Error('Duplicate containers');
const manifest = {
  upstream: UPSTREAM,
  format: 'Synthetic D3D12.X console compute shader containers (wave32, 32 x 1 x 1 threads)',
  generator: `Node ${process.version}, zlib ${process.versions.zlib}`,
  directSplitInstructions: 12,
  shaderCount: SHADER_COUNT,
  manifestSha256: sha256(files.map((file) => `${file.sha256}  ${file.name}\n`).join('')),
  files,
};
writeFileSync(join(root, 'corpus-manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Generated ${SHADER_COUNT} distinct synthetic containers: ${manifest.manifestSha256}`);
