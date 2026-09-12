import { Segmented, Select, Space, Tag, Tooltip } from 'antd';
import type { Candidate, LayoutMode, Perspective } from '@/entities/view';
import { truncate } from '@/shared/lib/format';

export interface BreadcrumbItem {
  perspective: string;
  label: string;
  node: number | null;
  nodeName: string;
}

/**
 * 两级筛选器。
 *
 * * 一级 = **视角**（路由 / 表 / 外部系统 / 领域聚合 …）
 * * 二级 = **对象**（该视角下的某一个具体对象）
 * * 聚合类视角不是单链路，因此**没有二级**
 *
 * 选中后只渲染「当前这一个对象」的链路；切换靠这里，而不是靠图里长出其它边。
 */
export function PerspectivePicker({
  perspectives,
  perspective,
  onPerspectiveChange,
  candidates,
  node,
  onNodeChange,
  layout,
  onLayoutChange,
  trail,
  onTrailClick,
  loading,
}: {
  perspectives: Perspective[];
  perspective: string | null;
  onPerspectiveChange: (id: string) => void;
  candidates: Candidate[];
  node: number | null;
  onNodeChange: (id: number) => void;
  layout: LayoutMode | null;
  onLayoutChange: (m: LayoutMode) => void;
  trail: BreadcrumbItem[];
  onTrailClick: (index: number) => void;
  loading?: boolean;
}) {
  const current = perspectives.find((p) => p.id === perspective) ?? null;
  const isAggregate = current?.mode === 'aggregate';

  return (
    <Space direction="vertical" size={10} style={{ width: '100%' }}>
      {/* 一级：视角 */}
      <Space size={10} wrap>
        <span style={{ fontSize: 12, color: 'rgba(0,0,0,0.45)', width: 52 }}>一级 · 视角</span>
        <Segmented
          value={perspective ?? undefined}
          onChange={(v) => onPerspectiveChange(String(v))}
          options={perspectives.map((p) => ({
            value: p.id,
            label: (
              <Tooltip title={p.description ?? p.label}>
                <span>
                  {p.label}
                  <span style={{ opacity: 0.5, fontSize: 11, marginLeft: 4 }}>
                    {p.available}
                  </span>
                </span>
              </Tooltip>
            ),
          }))}
        />
        {current ? (
          <Tag color={isAggregate ? 'purple' : 'blue'}>
            {isAggregate ? '聚合概览（非单链路）' : '单链路子图'}
          </Tag>
        ) : null}
      </Space>

      {/* 二级：对象 */}
      <Space size={10} wrap>
        <span style={{ fontSize: 12, color: 'rgba(0,0,0,0.45)', width: 52 }}>二级 · 对象</span>
        {isAggregate ? (
          <Tag>聚合视角没有"单个对象"，展示的是分组与计数</Tag>
        ) : (
          <Select
            showSearch
            style={{ width: 420 }}
            placeholder={loading ? '加载候选…' : '选择一个对象'}
            value={node ?? undefined}
            loading={loading}
            onChange={(v: number) => onNodeChange(v)}
            filterOption={(input, option) =>
              String(option?.label ?? '').toLowerCase().includes(input.toLowerCase())
            }
            options={candidates.map((c) => ({
              value: c.id,
              label: `${c.name}${c.badge ? ` · ${c.badge}` : ''}`,
            }))}
          />
        )}
        <Space size={6}>
          <span style={{ fontSize: 12, color: 'rgba(0,0,0,0.45)' }}>布局</span>
          <Select
            size="small"
            style={{ width: 130 }}
            value={layout ?? current?.layout ?? 'radial'}
            onChange={(v: LayoutMode) => onLayoutChange(v)}
            options={[
              { value: 'radial', label: '径向（环=跳数）' },
              { value: 'layered', label: '分层调用链' },
              { value: 'spine', label: 'Spine 取证' },
              { value: 'compound', label: '聚类框' },
              { value: 'matrix', label: '矩阵' },
              { value: 'er', label: 'ER 正交' },
            ]}
          />
        </Space>
      </Space>

      {/* 面包屑：可回退到任意一步 */}
      {trail.length > 1 ? (
        <Space size={4} wrap style={{ fontSize: 12 }}>
          <span style={{ color: 'rgba(0,0,0,0.45)' }}>回退：</span>
          {trail.map((t, i) => (
            <span key={`${t.perspective}-${t.node}-${i}`}>
              {i > 0 ? <span style={{ color: 'rgba(0,0,0,0.25)' }}> › </span> : null}
              <a
                onClick={() => onTrailClick(i)}
                style={{
                  fontWeight: i === trail.length - 1 ? 600 : 400,
                  color: i === trail.length - 1 ? '#0f172a' : '#3d7eff',
                }}
              >
                {t.label}
                {t.nodeName ? ` · ${truncate(t.nodeName, 28)}` : ''}
              </a>
            </span>
          ))}
        </Space>
      ) : null}
    </Space>
  );
}
