import { describe, it, expect } from 'vitest';
import { ideUrl, isSensitive, driftHint } from './ide';
import type { SourceLocation } from '@/entities/view';

const loc = (file: string, line: number, symbol: string | null): SourceLocation => ({
  file,
  line,
  symbol,
  note: null,
});

describe('ideUrl', () => {
  it('vscode 绝对路径', () => {
    expect(ideUrl('vscode', loc('/a/b.php', 10, null))).toBe('vscode://file//a/b.php:10');
  });

  it('vscode 相对路径拼接工程根', () => {
    expect(ideUrl('vscode', loc('src/x.php', 3, null), '/root')).toBe(
      'vscode://file//root/src/x.php:3',
    );
  });

  it('JetBrains 系追加符号', () => {
    const u = ideUrl('phpstorm', loc('/a/b.php', 10, 'Order'), '/root');
    expect(u).toContain('phpstorm');
    expect(u).toContain('Order');
  });
});

describe('isSensitive', () => {
  it('识别 SecretLocation', () => expect(isSensitive('SecretLocation')).toBe(true));
  it('普通节点非敏感', () => expect(isSensitive('Class')).toBe(false));
});

describe('driftHint', () => {
  it('有符号时提示按符号定位', () => {
    expect(driftHint(loc('/a', 1, 'Foo'))).toContain('Foo');
  });
  it('无符号时提示按文件路径定位', () => {
    expect(driftHint(loc('/a', 1, null))).toContain('文件路径');
  });
});
