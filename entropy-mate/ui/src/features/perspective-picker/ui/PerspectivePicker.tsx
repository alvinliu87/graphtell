import { useMemo } from 'react';
import { Select, Space, Tag, Tooltip } from 'antd';
import type { Candidate, LayoutMode, Perspective } from '@/entities/view';
import { useLocale } from '@/shared/lib/i18n';
import { truncate } from '@/shared/lib/format';

/** 布局算法可读名（用于「跟随视角默认」选项的说明）。 */
const LAYOUT_LABELS: Record<string, string> = {
  radial: '径向 / 星形自适应',
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
  nodeName,
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
  /**
   * 当前选中节点的**兜底显示名**。
   *
   * 点图导航是直接给节点 id（不经候选列表），而候选是按需加载的 —— 两者叠加时
   * 下拉里找不到匹配 `value` 的选项，antd 会把 value 原样渲染成裸 id（`57601`）。
   * 有了它就能补一个选项，显示名字；候选已加载且命中时本字段不生效。
   */
  nodeName?: string | null;
  layout: LayoutMode | null;
  /** 传 `null` 表示"跟随视角默认"，即清掉 URL 里的 `m` 覆盖。 */
  onLayoutChange: (m: LayoutMode | null) => void;
  trail: BreadcrumbItem[];
  onTrailClick: (index: number) => void;
  loading?: boolean;
}) {
  const current = perspectives.find((p) => p.id === perspective) ?? null;
  const isAggregate = current?.mode === 'aggregate';
  const { t } = useLocale();

  /**
   * 二级下拉的选项。
   *
   * 选中节点若不在候选里（点图导航后候选尚未加载，或价值排序把它挤出前 N），
   * 必须**补一个选项**，否则 antd 会把 `value` 直接渲染成裸 id。
   */
  const nodeOptions = useMemo(() => {
    const options = candidates.map((c) => ({
      value: c.id,
      label: `${c.name}${c.badge ? ` · ${c.badge}` : ''}`,
    }));
    if (node !== null && !candidates.some((c) => c.id === node)) {
      options.unshift({ value: node, label: nodeName || `#${node}` });
    }
    return options;
  }, [candidates, node, nodeName]);

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
          <Tag>{t('聚合视角没有"单个对象"')}</Tag>
        ) : (
          <Select
            showSearch
            style={{ width: 360 }}
            placeholder={loading ? t('加载候选…') : t('选择一个对象')}
            value={node ?? undefined}
            loading={loading}
            onChange={(v: number) => onNodeChange(v)}
            filterOption={false}
            onSearch={onSearch}
            onDropdownVisibleChange={onDropdownVisibleChange}
            options={nodeOptions}
          />
        )}
        <Space size={6}>
          <span style={{ fontSize: 12, color: 'rgba(0,0,0,0.45)' }}>{t('布局')}</span>
          <Select
            size="small"
            style={{ width: 168 }}
            value={(layout ?? AUTO) as string}
            onChange={(v: string) => onLayoutChange(v === AUTO ? null : (v as LayoutMode))}
            options={[
              {
                value: AUTO,
                label: t('跟随视角默认（') + t(LAYOUT_LABELS[current?.layout ?? 'radial'] ?? current?.layout ?? 'radial') + t('）'),
              },
              { value: 'radial', label: t('径向 / 星形自适应') },
              { value: 'layered', label: t('分层调用链') },
              { value: 'spine', label: t('Spine 取证') },
              { value: 'compound', label: t('聚类框') },
              { value: 'matrix', label: t('矩阵') },
              { value: 'er', label: t('ER 正交') },
            ]}
          />
        </Space>
        {current ? (
          <Tag color={isAggregate ? 'purple' : 'blue'}>
            {isAggregate ? t('聚合概览') : t('单链路')}
          </Tag>
        ) : null}
      </Space>

      {/* 面包屑：可回退到任意一步 */}
      {trail.length > 1 ? (
        <Space size={4} wrap style={{ fontSize: 12 }}>
          <span style={{ color: 'rgba(0,0,0,0.45)' }}>{t('回退：')}</span>
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
