import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
import { checkApi } from './api';

describe('checkApi', () => {
  beforeEach(() => vi.clearAllMocks());

  it('rules fetches the rule set', async () => {
    await checkApi.rules();
    expect(http.get).toHaveBeenCalledWith('/api/rules');
  });

  it('check sends null rule_ids when none are given', async () => {
    await checkApi.check(3);
    expect(http.post).toHaveBeenCalledWith('/api/projects/3/check', { rule_ids: null });
  });

  it('check passes the rule id list when provided', async () => {
    await checkApi.check(3, ['r1', 'r2']);
    expect(http.post).toHaveBeenCalledWith('/api/projects/3/check', { rule_ids: ['r1', 'r2'] });
  });

  it('violations builds limit and sub-project query params', async () => {
    await checkApi.violations(3, 200, [5, 9]);
    expect(http.get).toHaveBeenCalledWith(
      '/api/projects/3/violations?limit=200&sub_project_id=5,9',
    );
  });

  it('violations omits sub_project_id when empty', async () => {
    await checkApi.violations(3);
    expect(http.get).toHaveBeenCalledWith('/api/projects/3/violations?limit=500');
  });

  it('summary, ruleConfigs, putRuleConfig, batchRuleConfig, resetRuleConfig hit the right endpoints', async () => {
    await checkApi.summary(3);
    expect(http.get).toHaveBeenCalledWith('/api/projects/3/check/summary');

    await checkApi.ruleConfigs(3);
    expect(http.get).toHaveBeenCalledWith('/api/projects/3/rules/config');

    await checkApi.putRuleConfig(3, { rule_id: 'r1', enabled: false });
    expect(http.put).toHaveBeenCalledWith('/api/projects/3/rules/config', { rule_id: 'r1', enabled: false });

    await checkApi.batchRuleConfig(3, [{ rule_id: 'r1' }]);
    expect(http.post).toHaveBeenCalledWith('/api/projects/3/rules/config/batch', { items: [{ rule_id: 'r1' }] });

    await checkApi.resetRuleConfig(3, 'r1');
    expect(http.del).toHaveBeenCalledWith('/api/projects/3/rules/config/r1');
  });
});
