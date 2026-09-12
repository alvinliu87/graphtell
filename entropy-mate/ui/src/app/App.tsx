import { App as AntdApp, ConfigProvider } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import { RouterProvider } from 'react-router-dom';
import { router } from './router';

/** 应用根：主题 + 全局消息上下文 + 路由。 */
export function App() {
  return (
    <ConfigProvider
      locale={zhCN}
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
