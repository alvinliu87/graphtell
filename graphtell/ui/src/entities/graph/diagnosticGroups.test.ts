import { describe, expect, it } from 'vitest';
import {
  actionableCount,
  asSeverity,
  categoryOf,
  groupDiagnostics,
  sortGroups,
} from './diagnosticGroups';
import type { Diagnostic, DiagnosticCodeCount } from './model';

const diag = (code: string, severity: Diagnostic['severity'], location: string): Diagnostic => ({
  project_id: 1,
  phase: 'Resolve',
  code,
  severity,
  message: `${code} @ ${location}`,
  location,
  payload: null,
});

describe('diagnostic grouping', () => {
  it('aggregates by code; counts come from the full summary rather than the detail window', () => {
    // The detail window holds only 2 IdentityUnresolved rows while the summary says 349 -- it must report 349.
    const byCode: DiagnosticCodeCount[] = [
      { code: 'IdentityUnresolved', severity: 'warning', count: 349 },
      { code: 'UnresolvedLink', severity: 'warning', count: 21 },
      { code: 'UnresolvedLink', severity: 'info', count: 35 },
    ];
    const items = [
      diag('IdentityUnresolved', 'warning', 'a.vue:1'),
      diag('IdentityUnresolved', 'warning', 'b.vue:2'),
    ];
    const groups = groupDiagnostics(byCode, items);

    const identity = groups.find((g) => g.code === 'IdentityUnresolved')!;
    expect(identity.count).toBe(349);
    expect(identity.samples).toHaveLength(2);
    expect(identity.samples[0].location).toBe('a.vue:1');

    // Different severities of the same code merge into one class, but the distribution is kept: warning 21 + info 35.
    const link = groups.find((g) => g.code === 'UnresolvedLink')!;
    expect(link.count).toBe(56);
    expect(link.bySeverity).toEqual({ warning: 21, info: 35 });
    expect(link.severity).toBe('warning');
  });

  it('when the same code means different things at two severities, the heaviest tier decides the grouping', () => {
    // UnresolvedLink at warning level = a route pointing at a non-existent handler (worth a look); at info level = vendor, which is expected.
    expect(categoryOf('UnresolvedLink', 'warning')).toBe('actionable');
    expect(categoryOf('IdentityUnresolved', 'warning')).toBe('engine');
    expect(categoryOf('AliasTargetMissing', 'info')).toBe('expected');
  });

  it('an unlisted code doesn’t get a guessed meaning; it falls back by severity', () => {
    expect(categoryOf('SomeBrandNewCode', 'warning')).toBe('actionable');
    expect(categoryOf('SomeBrandNewCode', 'info')).toBe('expected');
  });

  it('when the summary is missing it falls back to counting the detail window (the count is low, but not empty)', () => {
    const items = [
      diag('IdentityUnresolved', 'warning', 'a.vue:1'),
      diag('IdentityUnresolved', 'warning', 'b.vue:2'),
      diag('AliasTargetMissing', 'info', 'c.php:3'),
    ];
    const groups = groupDiagnostics(undefined, items);
    expect(groups.map((g) => [g.code, g.count])).toEqual([
      ['IdentityUnresolved', 2],
      ['AliasTargetMissing', 1],
    ]);
  });

  it('ordering: severity first → count descending', () => {
    const groups = groupDiagnostics(
      [
        { code: 'AliasTargetMissing', severity: 'info', count: 30 },
        { code: 'UnresolvedLink', severity: 'warning', count: 21 },
        { code: 'IdentityUnresolved', severity: 'warning', count: 349 },
      ],
      [],
    );
    expect(groups.map((g) => g.code)).toEqual([
      'IdentityUnresolved',
      'UnresolvedLink',
      'AliasTargetMissing',
    ]);
    // Sorting is a pure function: the same data sorts identically every time.
    expect(sortGroups([...groups].reverse()).map((g) => g.code)).toEqual(
      groups.map((g) => g.code),
    );
  });

  it('the “worth a look” count only counts actionable classes', () => {
    const groups = groupDiagnostics(
      [
        { code: 'IdentityUnresolved', severity: 'warning', count: 349 },
        { code: 'UnresolvedLink', severity: 'warning', count: 21 },
        { code: 'AliasTargetMissing', severity: 'info', count: 30 },
      ],
      [],
    );
    expect(actionableCount(groups)).toBe(21);
  });

  it('an unknown severity string is treated as the lightest tier, without throwing', () => {
    expect(asSeverity('bogus')).toBe('info');
  });
});
