// Shared jsdom polyfills + act environment flag for component tests.
// (Only touches the DOM when running under jsdom; harmless under node.)
import { afterEach } from 'vitest';

// antd portals (Drawer/Modal/Popconfirm) mount on document.body; clear it between
// tests so component smoke tests stay isolated, mirroring @testing-library cleanup.
afterEach(() => {
  document.body.innerHTML = '';
});

if (typeof window !== 'undefined') {
  if (!window.matchMedia) {
    window.matchMedia = ((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    })) as unknown as typeof window.matchMedia;
  }

  class ResizeObserverStub {
    observe() {}
    unobserve() {}
    disconnect() {}
  }
  (window as unknown as { ResizeObserver: unknown }).ResizeObserver = ResizeObserverStub;

  class IntersectionObserverStub {
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
  }
  (window as unknown as { IntersectionObserver: unknown }).IntersectionObserver = IntersectionObserverStub;

  if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
  if (!(Element.prototype as unknown as { scrollTo?: unknown }).scrollTo) {
    (Element.prototype as unknown as { scrollTo: unknown }).scrollTo = () => {};
  }
}

globalThis.IS_REACT_ACT_ENVIRONMENT = true;
