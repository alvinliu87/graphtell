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
  it('vscode absolute path should not carry a double slash', () => {
    expect(ideUrl('vscode', loc('/a/b.php', 10, null))).toBe('vscode://file/a/b.php:10');
  });

  it('vscode relative path joins the project root', () => {
    expect(ideUrl('vscode', loc('src/x.php', 3, null), '/root')).toBe(
      'vscode://file/root/src/x.php:3',
    );
  });

  it('cursor relative path joins the project root and strips the leading slash', () => {
    expect(ideUrl('cursor', loc('src/x.php', 5, null), '/root')).toBe(
      'cursor://file/root/src/x.php:5',
    );
  });

  it('JetBrains family appends the symbol and keeps the leading slash', () => {
    const u = ideUrl('phpstorm', loc('/a/b.php', 10, 'Order'), '/root');
    expect(u).toContain('phpstorm');
    expect(u).toContain('Order');
    expect(u).toContain('path=%2Fa%2Fb.php');
  });

  it('under WSL mode VS Code uses the remote scheme', () => {
    const u = ideUrl('vscode', loc('/home/x/a.php', 10, null), '/home/x', 'Ubuntu');
    expect(u).toBe('vscode://vscode-remote/wsl+Ubuntu/home/x/a.php:10');
  });

  it('under WSL mode Cursor also uses the remote scheme', () => {
    const u = ideUrl('cursor', loc('/home/x/a.php', 5, null), '/home/x', 'Debian');
    expect(u).toContain('vscode-remote/wsl+Debian');
  });

  it('under WSL mode JetBrains adds \\wsl$\\distro UNC prefix', () => {
    const u = ideUrl('phpstorm', loc('/home/x/a.php', 10, 'Order'), '/home/x', 'Ubuntu');
    expect(u).toContain('path=%5C%5Cwsl%24%5CUbuntu%2Fhome%2Fx%2Fa.php');
  });

  it('without WSL it stays a plain file scheme', () => {
    expect(ideUrl('vscode', loc('/home/x/a.php', 10, null), '/home/x')).toBe(
      'vscode://file/home/x/a.php:10',
    );
  });
});

describe('resolveProjectRoot', () => {
  it('per-project override has the highest priority', () => {
    expect(resolveProjectRoot('/be', '/override', '\\wsl$\\Ubuntu{root}')).toBe('/override');
  });
  it('without an override, the template substitutes {root} against the backend root', () => {
    expect(resolveProjectRoot('/home/x/em', '', '\\wsl$\\Ubuntu{root}')).toBe(
      '\\wsl$\\Ubuntu/home/x/em',
    );
  });
  it('without an override or template it falls back to the backend root', () => {
    expect(resolveProjectRoot('/home/x/em')).toBe('/home/x/em');
  });
  it('with no backend root the template returns as-is', () => {
    expect(resolveProjectRoot(undefined, '', '/host{root}')).toBe('/host{root}');
  });
});

describe('absolutePath', () => {
  it('relative path joins the project root', () => {
    expect(absolutePath('a/b.ts', '/root')).toBe('/root/a/b.ts');
  });
  it('absolute path is not joined', () => {
    expect(absolutePath('/abs/b.ts', '/root')).toBe('/abs/b.ts');
  });
});

describe('isSensitive', () => {
  it('recognizes SecretLocation', () => expect(isSensitive('SecretLocation')).toBe(true));
  it('an ordinary node is not sensitive', () => expect(isSensitive('Class')).toBe(false));
});

