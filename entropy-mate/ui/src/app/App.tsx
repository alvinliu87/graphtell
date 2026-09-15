import { App as AntdApp, ConfigProvider } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import enUS from 'antd/locale/en_US';
import { RouterProvider } from 'react-router-dom';
import { router } from './router';
import { LocaleProvider, useLocale } from '@/shared/lib/i18n';

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
        <RouterProvider router={router} />
      </AntdApp>
    </ConfigProvider>
  );
}
