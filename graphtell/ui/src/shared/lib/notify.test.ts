import { afterEach, describe, expect, it, vi } from 'vitest';

afterEach(() => {
  vi.useRealTimers();
  vi.resetModules();
});

async function load() {
  return import('./notify');
}

describe('notify', () => {
  it('routes the message to the registered notifier', async () => {
    const { notify, setNotifier } = await load();
    const fn = vi.fn();
    setNotifier(fn);
    notify('boom');
    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith('boom');
  });

  it('does not throw when no notifier is registered', async () => {
    const { notify, setNotifier } = await load();
    setNotifier(null);
    expect(() => notify('x')).not.toThrow();
  });

  it('de-dupes bursts within the 5s window', async () => {
    const { notify, setNotifier } = await load();
    const fn = vi.fn();
    setNotifier(fn);
    notify('a');
    notify('b');
    notify('c');
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it('allows a new call after the window elapses', async () => {
    vi.useFakeTimers();
    const { notify, setNotifier } = await load();
    const fn = vi.fn();
    setNotifier(fn);
    notify('a'); // now = 0, first call
    vi.advanceTimersByTime(5000);
    notify('b'); // now = 5000, window elapsed -> second call
    expect(fn).toHaveBeenCalledTimes(2);
  });
});
