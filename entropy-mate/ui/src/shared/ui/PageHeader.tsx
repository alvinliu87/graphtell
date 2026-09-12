import type { ReactNode } from 'react';

/** 页面标题 + 右侧操作区。 */
export function PageHeader({ title, subtitle, extra }: { title: ReactNode; subtitle?: ReactNode; extra?: ReactNode }) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'flex-end',
        justifyContent: 'space-between',
        gap: 16,
        marginBottom: 20,
        flexWrap: 'wrap',
      }}
    >
      <div>
        <h2 style={{ margin: 0, fontSize: 22, fontWeight: 650, letterSpacing: '-0.01em' }}>{title}</h2>
        {subtitle ? (
          <div style={{ marginTop: 6, color: 'rgba(0,0,0,0.45)', fontSize: 13 }}>{subtitle}</div>
        ) : null}
      </div>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>{extra}</div>
    </div>
  );
}
