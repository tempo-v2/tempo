import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, realpathSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const ui = fileURLToPath(new URL('../../tempo-ui/', import.meta.url));
const core = fileURLToPath(new URL('../../tempo-core-components/', import.meta.url));
const corePackage = '@tempo-v2/core-components';
const requireFromUi = createRequire(path.join(ui, 'package.json'));
const ts = requireFromUi('typescript');

function* sourceFiles(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
        const file = path.join(directory, entry.name);
        if (entry.isDirectory() && !['dist', 'node_modules', 'test'].includes(entry.name)) {
            yield* sourceFiles(file);
        } else if (entry.isFile() && /\.[cm]?[jt]sx?$/.test(entry.name)
            && !entry.name.endsWith('.scenarios.js') && entry.name !== 'build.mjs') {
            yield file;
        }
    }
}

function inside(root, target) {
    const relative = path.relative(root, target);
    return relative !== '..' && !relative.startsWith('..' + path.sep) && !path.isAbsolute(relative);
}

function importedSpecifiers(file, content) {
    const source = ts.createSourceFile(file, content, ts.ScriptTarget.Latest, true);
    assert.equal(source.parseDiagnostics.length, 0, source.parseDiagnostics.map(diagnostic =>
        ts.flattenDiagnosticMessageText(diagnostic.messageText, '\n')
    ).join('\n'));

    const specifiers = [];
    function visit(node) {
        let argument;
        if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
            argument = node.moduleSpecifier;
        } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
            argument = node.moduleReference.expression;
        } else if (ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument)) {
            argument = node.argument.literal;
        } else if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword) {
            argument = node.arguments[0];
        }
        if (argument && ts.isStringLiteralLike(argument)) specifiers.push(argument.text);
        ts.forEachChild(node, visit);
    }
    visit(source);
    return specifiers;
}

function checkSource(root, file, content, allowed) {
    for (const specifier of importedSpecifiers(file, content)) {
        if (specifier.startsWith('.')) {
            const destination = path.resolve(path.dirname(file), specifier);
            const resolved = existsSync(destination) ? realpathSync(destination) : destination;
            assert.ok(inside(realpathSync(root), resolved), `${file} imports outside its package: ${specifier}`);
        } else {
            assert.ok(allowed(specifier), `${file} uses undeclared UI boundary: ${specifier}`);
        }
    }
}

function checkImports(root, sources, allowed) {
    for (const file of sources) checkSource(root, file, readFileSync(file, 'utf8'), allowed);
}

const reactImport = specifier => specifier === 'react' || specifier.startsWith('react/');

function installedCoreExports() {
    const installed = path.join(ui, 'node_modules', corePackage);
    assert.equal(realpathSync(installed), installed, 'Core must be installed from the UI tarball');
    const manifest = JSON.parse(readFileSync(path.join(installed, 'package.json'), 'utf8'));
    const lock = JSON.parse(readFileSync(path.join(ui, 'package-lock.json'), 'utf8'));
    const pin = lock.packages[`node_modules/${corePackage}`];
    assert.match(pin.resolved, /^file:vendor\/.+\.tgz$/);
    const tarball = readFileSync(path.join(ui, pin.resolved.slice('file:'.length)));
    assert.equal(pin.integrity, 'sha512-' + createHash('sha512').update(tarball).digest('base64'));
    assert.equal(manifest.name, corePackage);
    assert.equal(manifest.version, pin.version);
    assert.ok(manifest.exports['.'].import.startsWith('./dist/'));

    return specifier => {
        if (specifier !== corePackage && !specifier.startsWith(corePackage + '/')) return false;
        const subpath = '.' + specifier.slice(corePackage.length);
        const exports = manifest.exports;
        let target = exports[subpath];
        if (typeof target === 'object' && target !== null) target = target.import;
        if (target === undefined) {
            for (const [key, mapped] of Object.entries(exports)) {
                if (!key.endsWith('*') || typeof mapped !== 'string' || !mapped.includes('*')) continue;
                const prefix = key.slice(0, -1);
                const suffix = subpath.slice(prefix.length);
                if (subpath.startsWith(prefix) && suffix && !suffix.split('/').some(part => ['.', '..', 'node_modules'].includes(part))) {
                    target = mapped.replace('*', suffix);
                    break;
                }
            }
        }
        const destination = typeof target === 'string' ? path.resolve(installed, target) : null;
        return destination !== null && existsSync(destination) && inside(installed, realpathSync(destination));
    };
}

function appImports() {
    const publicCore = installedCoreExports();
    return specifier => reactImport(specifier)
        || specifier === 'react-dom/client'
        || specifier === 'chess.js'
        || publicCore(specifier);
}

test('Core presentation source stays inside Core and depends only on React and Chessground', () => {
    const presentation = specifier => reactImport(specifier) || specifier.startsWith('@lichess-org/chessground/');
    checkImports(core, [
        ...sourceFiles(path.join(core, 'src')),
        ...sourceFiles(path.join(core, 'board', 'src')),
    ], presentation);
});

test('Tempo UI consumes the installed Core artifact through its declared public imports', () => {
    checkImports(ui, sourceFiles(ui), appImports());
});

const fixture = (contents, extension = 'tsx') =>
    checkSource(ui, path.join(ui, 'import-guard-fixture.' + extension), contents, appImports());

test('JSX from attribute and text are not imports; public Core exports are allowed', () => {
    fixture(`
      import type { BoardProps } from "@tempo-v2/core-components";
      import { Board } from "@tempo-v2/core-components";
      import "@tempo-v2/core-components/styles.css";
      import piece from "@tempo-v2/core-components/assets/merida/wK.svg";
      export const Demo = (value: BoardProps) => <Board name="from">import "not-a-package"</Board>;
    `);
});

test('private Core subpaths, sibling escapes and unapproved imports are rejected', () => {
    assert.throws(() => fixture('export { Board } from "@tempo-v2/core-components/dist/index.js";'), /boundary: @tempo-v2\/core-components\/dist\/index\.js/);
    assert.throws(() => fixture('import("../tempo/README.md");'), /outside its package: \.\.\/tempo\/README\.md/);
    assert.throws(() => fixture('import { value } from "undeclared"; export const selected: string = value;', 'ts'), /boundary: undeclared/);
    assert.throws(() => fixture('export type { Private } from "undeclared-types";', 'd.ts'), /boundary: undeclared-types/);
});
