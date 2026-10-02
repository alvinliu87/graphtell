import { App as AntdApp, ConfigProvider } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import enUS from 'antd/locale/en_US';
import { useEffect } from 'react';
import { RouterProvider } from 'react-router-dom';
import { router } from './router';
import { LocaleProvider, useLocale } from '@/shared/lib/i18n';
import { ErrorBoundary } from '@/shared/ui/ErrorBoundary';
import { setNotifier } from '@/shared/lib/notify';

/** App root: language + theme + global message context + router. */
export function App() {
  return (
    <LocaleProvider>
      <LocaleAware />
    </LocaleProvider>
  );
}

/** The language drives antd component localisation (e.g. pagination / empty-state copy). */
function LocaleAware() {
  const { lang } = useLocale();
  return (
    <ConfigProvider
      locale={lang === 'zh-CN' ? zhCN : enUS}
      theme={{
        token: {
          colorPrimary: '#3d7eff',
          borderRadius: 10,
          fontSize: 14,
          colorBgLayout: '#f7f8fa',
        },
        components: {
          Card: { paddingLG: 20 },
          Table: { headerBg: '#fafbfc' },
        },
      }}
    >
      <AntdApp>
        <NotifierBridge />
        <ErrorBoundary>
          <RouterProvider router={router} />
        </ErrorBoundary>
      </AntdApp>
    </ConfigProvider>
    );
    }

    /**
    * Registers antd's `message.error` into the global notify bridge.
    *
    * Why: the fetch wrapper in `shared/api/http.ts` is a pure function with no React context, so it
    * cannot raise an antd `message` directly; and an antd `message` must come from the instance
    * provided by `<App>` (the static `message.error` loses its styling under a ConfigProvider theme).
    * So here, inside `<AntdApp>`, `App.useApp()` yields the correct message instance and registers it;
    * it is removed again on unmount (hot reload / app teardown) to avoid a dangling reference.
    */
    function NotifierBridge() {
    const { message } = AntdApp.useApp();
    useEffect(() => {
    setNotifier((m: string) => message.error(m));
    return () => setNotifier(null);
    }, [message]);
    return null;
    }
