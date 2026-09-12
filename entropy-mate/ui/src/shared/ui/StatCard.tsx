import { Card, Statistic } from 'antd';
import type { ReactNode } from 'react';

/** 概览卡片：数字 + 图标。 */
export function StatCard({
  title,
  value,
  icon,
  accent = '#3d7eff',
  suffix,
}: {
  title: string;
  value: number | string;
  icon?: ReactNode;
  accent?: string;
  suffix?: string;
}) {
  return (
    <Card
      variant="borderless"
      style={{
        borderRadius: 14,
        boxShadow: '0 1px 2px rgba(16,24,40,0.06), 0 1px 3px rgba(16,24,40,0.06)',
        background: `linear-gradient(180deg, ${accent}0f 0%, #fff 62%)`,
      }}
      styles={{ body: { padding: 18 } }}
    >
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <Statistic
          title={<span style={{ fontSize: 13, color: 'rgba(0,0,0,0.55)' }}>{title}</span>}
          value={value}
          suffix={suffix}
          valueStyle={{ fontSize: 26, fontWeight: 650, color: accent }}
        />
        {icon ? (
          <div
            style={{
              width: 40,
              height: 40,
              borderRadius: 12,
              display: 'grid',
              placeItems: 'center',
              background: `${accent}1a`,
              color: accent,
              fontSize: 20,
            }}
          >
            {icon}
          </div>
        ) : null}
      </div>
    </Card>
  );
}
