import { describe, it, expect } from 'vitest';
import { ideUrl, isSensitive, resolveProjectRoot, absolutePath } from './ide';
import type { SourceLocation } from '@/entities/view';

const loc = (file: string, line: number, symbol: string | null): SourceLocation => ({
  file,
  line,
  symbol,
  note: null,
});

describe('ideUrl', () => {
  it('vscode 绝对路径不应带双重斜杠', () => {
    expect(ideUrl('vscode', loc('/a/b.php', 10, null))).toBe('vscode://file/a/b.php:10');
  });

  it('vscode 相对路径拼接工程根', () => {
    expect(ideUrl('vscode', loc('src/x.php', 3, null), '/root')).toBe(
      'vscode://file/root/src/x.php:3',
    );
  });

  it('cursor 相对路径拼接工程根并去掉前导斜杠', () => {
    expect(ideUrl('cursor', loc('src/x.php', 5, null), '/root')).toBe(
      'cursor://file/root/src/x.php:5',
    );
  });

  it('JetBrains 系追加符号并保留前导斜杠', () => {
    const u = ideUrl('phpstorm', loc('/a/b.php', 10, 'Order'), '/root');
    expect(u).toContain('phpstorm');
    expect(u).toContain('Order');
    expect(u).toContain('path=%2Fa%2Fb.php');
  });

  it('WSL 模式下 VS Code 走远程 scheme', () => {
    const u = ideUrl('vscode', loc('/home/x/a.php', 10, null), '/home/x', 'Ubuntu');
    expect(u).toBe('vscode://vscode-remote/wsl+Ubuntu/home/x/a.php:10');
  });

  it('WSL 模式下 Cursor 同样走远程 scheme', () => {
    const u = ideUrl('cursor', loc('/home/x/a.php', 5, null), '/home/x', 'Debian');
    expect(u).toContain('vscode-remote/wsl+Debian');
  });

  it('WSL 模式下 JetBrains 补 \\wsl$\\distro UNC 前缀', () => {
    const u = ideUrl('phpstorm', loc('/home/x/a.php', 10, 'Order'), '/home/x', 'Ubuntu');
    expect(u).toContain('path=%5C%5Cwsl%24%5CUbuntu%2Fhome%2Fx%2Fa.php');
  });

  it('无 WSL 时仍是普通 file scheme', () => {
    expect(ideUrl('vscode', loc('/home/x/a.php', 10, null), '/home/x')).toBe(
      'vscode://file/home/x/a.php:10',
    );
  });
});

describe('resolveProjectRoot', () => {
  it('按工程覆盖优先级最高', () => {
    expect(resolveProjectRoot('/be', '/override', '\\wsl$\\Ubuntu{root}')).toBe('/override');
  });
  it('无覆盖时用模板对后端根做 {root} 替换', () => {
    expect(resolveProjectRoot('/home/x/em', '', '\\wsl$\\Ubuntu{root}')).toBe(
      '\\wsl$\\Ubuntu/home/x/em',
    );
  });
  it('无覆盖无模板时回退后端根', () => {
    expect(resolveProjectRoot('/home/x/em')).toBe('/home/x/em');
  });
  it('无后端根时模板原样返回', () => {
    expect(resolveProjectRoot(undefined, '', '/host{root}')).toBe('/host{root}');
  });
});

describe('absolutePath', () => {
  it('相对路径拼接工程根', () => {
    expect(absolutePath('a/b.ts', '/root')).toBe('/root/a/b.ts');
  });
  it('绝对路径不拼接', () => {
    expect(absolutePath('/abs/b.ts', '/root')).toBe('/abs/b.ts');
  });
});

describe('isSensitive', () => {
  it('识别 SecretLocation', () => expect(isSensitive('SecretLocation')).toBe(true));
  it('普通节点非敏感', () => expect(isSensitive('Class')).toBe(false));
});

