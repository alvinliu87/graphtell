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
 * Global render error boundary.
 *
 * Why it is needed: the UI previously had **no** React error boundary at all (no `componentDidCatch`
 * anywhere in `ui/src`). Once any component throws during render (malformed JSON from the backend,
 * an undefined field, a broken antd usage), React bubbles it all the way to the root, and the result
 * is a **completely blank page with no degradation** — harder to diagnose than a backend 500.
 *
 * Where it sits: inside `<AntdApp>` and outside `<RouterProvider>` (see `app/App.tsx`), so the
 * fallback UI still gets antd's `App` context and locale. If the crash originates inside antd itself
 * this boundary cannot catch it either, but in practice render crashes almost always come from
 * business components / the data layer.
 *
 * Scope: this only does "do not go blank + offer a recovery path + log the error to the console".
 * The real root cause (type mismatch / backend contract drift) is left to the browser console and
 * backend logs, not papered over in the frontend.
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Render-time crashes are only contained here; the root cause lives in the browser console and backend logs.
    console.error('[ErrorBoundary]', error, info.componentStack);
  }

  private reload = () => window.location.reload();

  render() {
    const { error } = this.state;
    if (error) {
      return (
        <Result
          status="error"
          title={translate('Something went wrong')}
          subTitle={translate('An unexpected error occurred while rendering. It has been contained to avoid taking down the whole app. Reload to recover, or open the browser console for the full stack.')}
          extra={[
            <Button type="primary" icon={<ReloadOutlined />} onClick={this.reload}>
              {translate('Reload')}
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
