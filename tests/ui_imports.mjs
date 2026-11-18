import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const ui = fileURLToPath(new URL('../../tempo-ui/', import.meta.url));
const core = fileURLToPath(new URL('../../tempo-core-components/', import.meta.url));
const corePackage = '@tempo-v2/core-components';

function checkImports(root, source, allowed) {
    for (const entry of readdirSync(source, { withFileTypes: true })) {
        const file = path.join(source, entry.name);
        if (entry.isDirectory() && !['dist', 'node_modules', 'test'].includes(entry.name)) {
            checkImports(root, file, allowed);
        }
        if (!entry.isFile() || !/\.[jt]sx?$/.test(entry.name) || entry.name.endsWith('.scenarios.js')) {
            continue;
        }
        const content = readFileSync(file, 'utf8');
        for (const [, specifier] of content.matchAll(/(?:\b(?:import|from)\s*(?:\(\s*)?)["']([^"']+)["']/g)) {
            if (specifier.startsWith('.')) {
                const relative = path.relative(root, path.resolve(source, specifier));
                assert.ok(!relative.startsWith('..'), file + ' imports outside its package: ' + specifier);
            } else {
                assert.ok(allowed(specifier), file + ' uses undeclared UI boundary: ' + specifier);
            }
        }
    }
}

const reactImport = specifier => specifier === 'react' || specifier.startsWith('react/');

test('Core presentation source stays inside Core and depends only on React and Chessground', () => {
    const presentation = specifier => reactImport(specifier) || specifier.startsWith('@lichess-org/chessground/');
    checkImports(core, path.join(core, 'src'), presentation);
    checkImports(core, path.join(core, 'board', 'src'), presentation);
});

test('Tempo UI consumes the Core artifact through its declared public imports', () => {
    const exports = JSON.parse(readFileSync(path.join(core, 'package.json'), 'utf8')).exports;
    const appImport = specifier =>
        reactImport(specifier)
        || specifier === 'react-dom/client'
        || specifier === corePackage && '.' in exports
        || specifier === corePackage + '/styles.css' && './styles.css' in exports;
    checkImports(ui, ui, appImport);
    assert.ok(exports['.'].import.startsWith('./dist/'));
});
