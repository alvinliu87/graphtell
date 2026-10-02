import type { ReactNode } from 'react';

/** Page title + right-hand action area. */
export function PageHeader({
  title,
  subtitle,
  extra,
  /** Compact mode: the page header is the first place to cut "height above the graph" — tighter margins and subtitle spacing. */
  compact,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  extra?: ReactNode;
  compact?: boolean;
}) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'flex-end',
        justifyContent: 'space-between',
        gap: 16,
        marginBottom: compact ? 10 : 20,
        flexWrap: 'wrap',
      }}
    >
      <div>
        <h2 style={{ margin: 0, fontSize: compact ? 18 : 22, fontWeight: 650, letterSpacing: '-0.01em' }}>{title}</h2>
        {subtitle ? (
          <div
            style={{
              marginTop: compact ? 2 : 6,
              color: 'rgba(0,0,0,0.45)',
              fontSize: compact ? 12 : 13,
            }}
          >
            {subtitle}
          </div>
        ) : null}
      </div>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>{extra}</div>
    </div>
  );
}
