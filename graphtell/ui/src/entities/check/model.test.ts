import { describe, expect, it } from 'vitest';

import {
  SEVERITY_COLOR,
  SEVERITY_LABEL,
  SEVERITY_ORDER,
  SEVERITY_RANK,
  ruleCategoryLabel,
} from './model';

describe('check model — severity metadata', () => {
  it('orders severities most-to-least urgent', () => {
    expect(SEVERITY_ORDER).toEqual(['critical', 'error', 'warning', 'info']);
  });

  it('ranks critical above info', () => {
    expect(SEVERITY_RANK.critical).toBeLessThan(SEVERITY_RANK.info);
    expect(SEVERITY_RANK.error).toBeLessThan(SEVERITY_RANK.warning);
  });

  it('has a label and colour for every severity tier', () => {
    for (const s of ['critical', 'error', 'warning', 'info'] as const) {
      expect(SEVERITY_LABEL[s]).toBeTruthy();
      expect(SEVERITY_COLOR[s]).toBeTruthy();
    }
  });
});

describe('check model — rule category labels', () => {
  it('maps a known category slug to its plain-language label', () => {
    expect(ruleCategoryLabel('api-hygiene')).toBe('API hygiene');
    expect(ruleCategoryLabel('security')).toBe('Security');
  });

  it('falls back to the slug itself for unknown categories', () => {
    expect(ruleCategoryLabel('some-new-fkb-category')).toBe('some-new-fkb-category');
  });
});
