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

describe('诊断归类', () => {
  it('按 code 聚合，计数取自全量汇总而不是明细窗口', () => {
    // 明细窗口里只有 2 条 IdentityUnresolved，汇总说 349 条 —— 必须报 349。
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

    // 同一 code 的不同严重度合成一类，但分布保留：警告 21 + 提示 35。
    const link = groups.find((g) => g.code === 'UnresolvedLink')!;
    expect(link.count).toBe(56);
    expect(link.bySeverity).toEqual({ warning: 21, info: 35 });
    expect(link.severity).toBe('warning');
  });

  it('同一 code 在两种严重度下含义不同时，最重的一档决定分类', () => {
    // UnresolvedLink 警告级 = 路由指向不存在的 handler（值得看）；提示级 = vendor，属预期。
    expect(categoryOf('UnresolvedLink', 'warning')).toBe('actionable');
    expect(categoryOf('IdentityUnresolved', 'warning')).toBe('engine');
    expect(categoryOf('AliasTargetMissing', 'info')).toBe('expected');
  });

  it('未收录的 code 不猜含义，按严重度兜底', () => {
    expect(categoryOf('SomeBrandNewCode', 'warning')).toBe('actionable');
    expect(categoryOf('SomeBrandNewCode', 'info')).toBe('expected');
  });

  it('汇总缺失时退回数明细窗口（计数偏小，但不空）', () => {
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

  it('排序：严重度优先 → 条数降序', () => {
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
    // 排序是纯函数：同一份数据多次排序结果一致。
    expect(sortGroups([...groups].reverse()).map((g) => g.code)).toEqual(
      groups.map((g) => g.code),
    );
  });

  it('「值得看一眼」的条数只算 actionable 类', () => {
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

  it('严重度字符串未知时按最轻一档处理，不抛错', () => {
    expect(asSeverity('bogus')).toBe('info');
  });
});
