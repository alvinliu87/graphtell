import { App as AntdApp, ConfigProvider } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import enUS from 'antd/locale/en_US';
import { useEffect } from 'react';
import { RouterProvider } from 'react-router-dom';
import { router } from './router';
import { LocaleProvider, useLocale } from '@/shared/lib/i18n';
import { ErrorBoundary } from '@/shared/ui/ErrorBoundary';
import { setNotifier } from '@/shared/lib/notify';

/** 应用根：语言 + 主题 + 全局消息上下文 + 路由。 */
export function App() {
  return (
    <LocaleProvider>
      <LocaleAware />
    </LocaleProvider>
  );
}

/** 语言决定 antd 组件的区域化（如分页 / 空态文案）。 */
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
    * 把 antd 的 `message.error` 注册进全局 notify 桥。
    *
    * 原因：`shared/api/http.ts` 的 fetch 封装是纯函数，拿不到 React 上下文，无法直接弹
    * antd 的 message；而 antd 的 message 又必须用 `<App>` 提供的实例（静态 `message.error`
    * 在用了 ConfigProvider 主题时会丢样式）。所以这里在 `<AntdApp>` 内部用 `App.useApp()`
    * 拿到正确的 message 实例并注册；组件卸载（热更新 / 应用卸载）时撤掉，避免野指针。
    */
    function NotifierBridge() {
    const { message } = AntdApp.useApp();
    useEffect(() => {
    setNotifier((m: string) => message.error(m));
    return () => setNotifier(null);
    }, [message]);
    return null;
    }
