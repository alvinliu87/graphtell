import { Select, Space, Tag, Tooltip } from 'antd';
import type { Candidate, LayoutMode, Perspective } from '@/entities/view';
import { truncate } from '@/shared/lib/format';

/** 布局算法可读名（用于「跟随视角默认」选项的说明）。 */
const LAYOUT_LABELS: Record<string, string> = {
  radial: '径向（环=跳数）',
  layered: '分层调用链',
  spine: 'Spine 取证',
  compound: '聚类框',
  matrix: '矩阵',
  er: 'ER 正交',
};

/** 「跟随视角默认」在 Select 里的哨兵值（不是合法 LayoutMode）。 */
const AUTO = '__auto__' as const;

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
  onSearch,
  onDropdownVisibleChange,
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
  onSearch?: (input: string) => void;
  /** 二级对象下拉展开/收起时回调，供上层做"按需加载候选"。 */
  onDropdownVisibleChange?: (open: boolean) => void;
  layout: LayoutMode | null;
  /** 传 `null` 表示"跟随视角默认"，即清掉 URL 里的 `m` 覆盖。 */
  onLayoutChange: (m: LayoutMode | null) => void;
  trail: BreadcrumbItem[];
  onTrailClick: (index: number) => void;
  loading?: boolean;
}) {
  const current = perspectives.find((p) => p.id === perspective) ?? null;
  const isAggregate = current?.mode === 'aggregate';

  return (
    <Space direction="vertical" size={10} style={{ width: '100%' }}>
      {/* 一行：一级视角（下拉）+ 二级对象 + 布局，紧凑成一行，减少竖向占用 */}
      <Space size={10} wrap align="center">
        <Select
          style={{ width: 200 }}
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
        {isAggregate ? (
          <Tag>聚合视角没有"单个对象"</Tag>
        ) : (
          <Select
            showSearch
            style={{ width: 360 }}
            placeholder={loading ? '加载候选…' : '选择一个对象'}
            value={node ?? undefined}
            loading={loading}
            onChange={(v: number) => onNodeChange(v)}
            filterOption={false}
            onSearch={onSearch}
            onDropdownVisibleChange={onDropdownVisibleChange}
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
            style={{ width: 168 }}
            value={(layout ?? AUTO) as string}
            onChange={(v: string) => onLayoutChange(v === AUTO ? null : (v as LayoutMode))}
            options={[
              {
                value: AUTO,
                label: `跟随视角默认（${LAYOUT_LABELS[current?.layout ?? 'radial'] ?? current?.layout ?? 'radial'}）`,
              },
              { value: 'radial', label: '径向（环=跳数）' },
              { value: 'layered', label: '分层调用链' },
              { value: 'spine', label: 'Spine 取证' },
              { value: 'compound', label: '聚类框' },
              { value: 'matrix', label: '矩阵' },
              { value: 'er', label: 'ER 正交' },
            ]}
          />
        </Space>
        {current ? (
          <Tag color={isAggregate ? 'purple' : 'blue'}>
            {isAggregate ? '聚合概览' : '单链路'}
          </Tag>
        ) : null}
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
