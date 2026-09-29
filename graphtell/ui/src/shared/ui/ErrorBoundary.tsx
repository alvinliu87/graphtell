import { Component, type ErrorInfo, type ReactNode } from 'react';
import { Button, Result } from 'antd';
import { ReloadOutlined } from '@ant-design/icons';
import { translate } from '@/shared/lib/i18n';

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * 全局渲染错误边界。
 *
 * 为什么需要：整个 UI 此前**没有任何** React 错误边界（`ui/src` 里搜不到 `componentDidCatch`）。
 * 一旦某个组件的渲染期抛错（后端返回畸形 JSON、字段 undefined、某个 antd 用法炸了），
 * React 会一路冒泡到根节点，结果是**整页白屏且无任何降级**——比后端 500 更难排查。
 *
 * 放哪：挂在 `<AntdApp>` 之内、`<RouterProvider>` 之外（见 `app/App.tsx`），这样兜底 UI
 * 仍能享用 antd 的 `App` 上下文与区域化；崩溃源若恰好在 antd 本身则另说，那种情况本边界也兜不住，
 * 但实践中渲染崩溃几乎都来自业务组件 / 数据层。
 *
 * 职责边界：这里只做"别白屏 + 给个恢复入口 + 把错误打到控制台"。真正的根因
 * （类型错配 / 后端契约漂移）靠浏览器控制台与后端日志兜底，不在前端美化。
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // 渲染期崩溃只在前端兜底，根因靠浏览器控制台与后端日志。
    console.error('[ErrorBoundary]', error, info.componentStack);
  }

  private reload = () => window.location.reload();

  render() {
    const { error } = this.state;
    if (error) {
      return (
        <Result
          status="error"
          title={translate('页面出错了')}
          subTitle={translate(
            '页面渲染时发生意外错误，已阻止其影响整个应用。可重新加载恢复，或查看浏览器控制台获取详细堆栈。',
          )}
          extra={[
            <Button type="primary" icon={<ReloadOutlined />} onClick={this.reload}>
              {translate('重新加载')}
            </Button>,
          ]}
        >
          <pre
            style={{
              textAlign: 'left',
              whiteSpace: 'pre-wrap',
              wordBreak: 'break-word',
              maxHeight: 260,
              overflow: 'auto',
              margin: 0,
            }}
          >
            <code>{error.message || String(error)}</code>
          </pre>
        </Result>
      );
    }
    return this.props.children;
  }
}
