// PR-comment projection contract using the actual dashboard click handler.
// Run: node tools/tests/test_clipboard_contract.js
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '../..');
const asset = fs.readFileSync(path.join(root, 'src/artifacts/dashboard/assets.rs'), 'utf8');
const source = asset.slice(asset.indexOf('    function decisionReadoutComment(report)'), asset.indexOf('    // -- Review/Author view toggle (PRV-105) --'));
function copy(report, lang = 'en') {
    const strings = JSON.parse(fs.readFileSync(path.join(root, 'locales', lang + '.json'), 'utf8'));
    let click, captured;
    const button = {addEventListener(type, handler) { assert.equal(type, 'click'); click = handler; }};
    const sandbox = {
        t: key => strings[key], setTimeout() {},
        document: {getElementById(id) { return id === 'copy-pr-comment' ? button : {textContent: JSON.stringify(report)}; }},
        navigator: {clipboard: {writeText(text) { captured = text; return Promise.resolve(); }}},
    };
    vm.runInNewContext(source, sandbox);
    click();
    return captured;
}
const details = {details_complete: true, automatic_merge: true, introduced_failures: 0,
    preexisting_failures: 1, mixed_failures: 0, unclassified_failures: 0};
const fixture = {gate: {allow_merge: false, recommended_merge: true, quality_pass: true, status: 'CONDITIONAL'},
    decision_readout: details, checks: [{id: 'semgrep_scan', status: 'FAIL'}, {id: 'cargo_audit', status: 'PASS'}]};
// Conflicting recommendation and readout permission cannot override gate.allow_merge.
assert.match(copy(fixture), /Automatic merge: NO · Introduced failures: 0 · Pre-existing failures: 1/);
assert.match(copy(fixture), /All checks:\*\* 1 passed, 1 failed, 0 with warnings/);
assert.match(copy(fixture), /Semgrep source scan:\*\* FAIL/);
assert.match(copy(fixture), /Cargo dependency audit:\*\* PASS/);
assert.match(copy(fixture, 'pl'), /Automatyczny merge: NIE · Wprowadzone błędy: 0 · Wcześniejsze błędy: 1/);
const clean = {...fixture, gate: {...fixture.gate, allow_merge: true, status: 'PASS'},
    decision_readout: {...details, preexisting_failures: 0}};
assert.match(copy(clean), /Automatic merge: YES · Introduced failures: 0/);
const warnings = {...clean, checks: [{id: 'cargo_geiger', status: 'WARN'}, {id: 'clippy', status: 'WARN'}]};
assert.match(copy(warnings), /Introduced failures: 0/);
assert.match(copy(warnings), /All checks:\*\* 0 passed, 0 failed, 2 with warnings/);
assert.doesNotMatch(copy(warnings), /Advisory warnings/);
assert.match(copy({...fixture, decision_readout: {...details, introduced_failures: 1}}), /Automatic merge: NO · Introduced failures: 1/);
assert.match(copy({...fixture, decision_readout: {...details, mixed_failures: 1, unclassified_failures: 1}}), /Mixed failures: 1 · Unclassified failures: 1/);
assert.match(copy({...fixture, decision_readout: {...details, details_complete: false}}), /Introduced failures: UNKNOWN/);
for (const allowed of [false, true]) {
    const expected = allowed ? /Automatic merge: YES · Introduced failures: UNKNOWN/ : /Automatic merge: NO · Introduced failures: UNKNOWN/;
    assert.match(copy({...fixture, gate: {allow_merge: allowed}, decision_readout: undefined}), expected);
}
const duplicates = {...fixture, checks: [
    {id: 'semgrep_scan', status: 'PASS'}, {id: 'semgrep_scan', status: 'FAIL'},
    {id: 'cargo_audit', status: 'PASS'}, {id: 'cargo_audit', status: 'ERROR'}]};
assert.match(copy(duplicates), /Semgrep source scan:\*\* PASS, FAIL/);
assert.match(copy(duplicates), /Cargo dependency audit:\*\* PASS, ERROR/);
const missingStatus = {...fixture, checks: [{id: 'semgrep_scan', status: 'PASS'}, {id: 'semgrep_scan'}, {id: 'cargo_audit', status: null}]};
assert.match(copy(missingStatus), /Semgrep source scan:\*\* PASS, UNKNOWN/);
assert.match(copy(missingStatus), /Cargo dependency audit:\*\* UNKNOWN/);
assert.match(copy({...fixture, checks: [{status: 'PASS'}]}), /Semgrep source scan:\*\* UNKNOWN/);
assert.match(copy({...fixture, checks: [{status: 'PASS'}]}), /Cargo dependency audit:\*\* UNKNOWN/);

const skipped = {...fixture, checks: [], security_scope: {source: 'recorded_check_results', checks: [
    {id: 'semgrep_scan', execution: 'not_executed', status: 'skipped', reason: 'security disabled'}]}};
assert.match(copy(skipped), /Semgrep source scan:\*\* NOT_RUN/);
assert.match(copy(skipped), /Cargo dependency audit:\*\* UNKNOWN/);
assert.match(copy({...skipped, checks: [{id: 'semgrep_scan', status: 'FAIL'}]}), /Semgrep source scan:\*\* FAIL/);
assert.match(copy({...skipped, security_scope: {...skipped.security_scope, source: 'requested_config'}}), /Semgrep source scan:\*\* UNKNOWN/);
console.log('clipboard-contract: PASS (canonical permission, failure counts, actual WARN totals, UNKNOWN, duplicates/missing security rows, EN/PL)');
