// One-off Codama codegen: Anchor IDLs -> Kit-native TS clients.
// Run: node codegen.mjs
import { rootNodeFromAnchor } from '@codama/nodes-from-anchor';
import { createFromRoot } from 'codama';
import { renderVisitor } from '@codama/renderers-js';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const idlDir = path.join(__dirname, '..', 'solana', 'idl');

for (const program of ['stock_vault', 'backstop']) {
  const idl = JSON.parse(readFileSync(path.join(idlDir, `${program}.json`), 'utf-8'));
  const codama = createFromRoot(rootNodeFromAnchor(idl));
  const outDir = path.join(__dirname, 'src', 'generated', program);
  codama.accept(
    renderVisitor(outDir, {
      formatCode: true,
      syncPackageJson: false,
      generatedFolder: '.',
      erasableSyntax: true,
    }),
  );
  console.log(`generated ${program} -> ${outDir}`);
}
