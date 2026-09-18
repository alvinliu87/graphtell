import { useMemo, type ReactNode } from 'react';
import { Select, Space, Tag, Tooltip } from 'antd';
import type { Candidate, Perspective } from '@/entities/view';
import { useLocale } from '@/shared/lib/i18n';
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
  onSearch,
  onDropdownVisibleChange,
  nodeName,
  trail,
  onTrailClick,
  loading,
  /** 追加在筛选行末尾的操作区（把「适应屏幕 / 结论·导航」等塞进同一行，
   *  避免图上方再占一条工具栏 —— 每多一行，图就要往上挤 40px）。 */
  extra,
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
  trail: BreadcrumbItem[];
  onTrailClick: (index: number) => void;
  loading?: boolean;
  extra?: ReactNode;
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

  // 面包屑只保留最近几步：它是一条"可回退"的辅助信息，宽屏也放不下十几步，
  // 而最该能点的是**最近**几步；更早的用「…」表示存在但不占宽度。
  const MAX_TRAIL = 3;
  const shownTrail = trail.length > MAX_TRAIL ? trail.slice(-MAX_TRAIL) : trail;
  const trailOffset = trail.length - shownTrail.length;

  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
      {/* 一行：一级视角（下拉）+ 二级对象 + 布局 + 面包屑 + 操作区 */}
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
      </Space>

      {/* 面包屑：可回退到任意一步。与筛选器同一行，超宽时**横向裁剪**而不是换行，
          换行会把下面的画布整体往下推。 */}
      {trail.length > 1 ? (
        <div
          style={{
            flex: '1 1 120px',
            minWidth: 0,
            display: 'flex',
            alignItems: 'center',
            gap: 4,
            fontSize: 12,
            whiteSpace: 'nowrap',
            overflow: 'hidden',
          }}
        >
          <span style={{ color: 'rgba(0,0,0,0.45)' }}>{t('回退：')}</span>
          {trailOffset > 0 ? <span style={{ color: 'rgba(0,0,0,0.25)' }}>… ›</span> : null}
          {shownTrail.map((item, i) => {
            const index = trailOffset + i;
            // 当前这一步"点了也白点"（= 原地不动），所以不当链接渲染：不显示手型指针，
            // 也不暗示它有跳转。
            const isCurrentStep = index === trail.length - 1;
            const text = (
              <>
                {item.label}
                {item.nodeName ? ` · ${truncate(item.nodeName, 28)}` : ''}
              </>
            );
            return (
              <span key={`${item.perspective}-${item.node}-${index}`}>
                {i > 0 ? <span style={{ color: 'rgba(0,0,0,0.25)' }}> › </span> : null}
                {isCurrentStep ? (
                  <span style={{ fontWeight: 600, color: '#0f172a' }}>{text}</span>
                ) : (
                  <a onClick={() => onTrailClick(index)} style={{ color: '#3d7eff' }}>
                    {text}
                  </a>
                )}
              </span>
            );
          })}
        </div>
      ) : null}

      {extra ? <div style={{ marginLeft: 'auto', display: 'flex', gap: 8, alignItems: 'center' }}>{extra}</div> : null}
    </div>
  );
}
