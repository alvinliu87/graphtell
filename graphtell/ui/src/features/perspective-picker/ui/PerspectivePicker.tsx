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
 * Two-level filter.
 *
 * * level 1 = **perspective** (routes / tables / external systems / domain aggregates …)
 * * level 2 = **object** (one concrete object under that perspective)
 * * aggregate perspectives aren't a single chain, so they have **no level 2**
 *
 * After selecting, render only "this one object"'s chain; switching happens here, not by growing other edges in the graph.
 */
export function PerspectivePicker({
  perspectives,
  perspective,
  onPerspectiveChange,
  candidates,
  node,
  onNodeChange,
  onSearch,
  onOpenChange,
  nodeName,
  /** Current level-2 search term. When non-empty the dropdown is "searching", and the fallback option shows only if its name matches the term. */
  searchText,
  trail,
  onTrailClick,
  loading,
  /** An action area appended at the end of the filter row (fitting "fit to screen / conclusions·navigation" into the same row
   *  to avoid another toolbar line above the graph -- every extra line pushes the graph up 40px). */
  extra,
}: {
  perspectives: Perspective[];
  perspective: string | null;
  onPerspectiveChange: (id: string) => void;
  candidates: Candidate[];
  node: number | null;
  onNodeChange: (id: number) => void;
  onSearch?: (input: string) => void;
  /** Callback when the level-2 dropdown opens/closes, for the parent to "load candidates on demand". */
  onOpenChange?: (open: boolean) => void;
  /** Current search-box input; non-empty means the user is searching, see the fallback filtering in `nodeOptions`. */
  searchText?: string;
  /**
   * The **fallback display name** of the currently selected node.
   *
   * Graph navigation passes a node id directly (not via the candidate list), and candidates are loaded on demand -- combined,
   * the dropdown has no option matching `value`, and antd renders the value as a bare id (`57601`).
   * This lets us add an option showing the name; it has no effect once candidates are loaded and match.
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
   * Options for the level-2 dropdown.
   *
   * If the selected node isn't in the candidates (candidates not yet loaded after graph navigation, or value ranking pushed it out of the top N),
   * an option **must be added**, otherwise antd renders `value` as a bare id.
   *
   * But while the user is **searching** (`searchText` non-empty), this fallback option shows only if its name also matches the search term --
   * otherwise when the backend finds nothing, the dropdown would show a "current selection" unrelated to the term,
   * looking like the search matched wrongly (e.g. typing `1` shows `store_product_services`).
   */
  const nodeOptions = useMemo(() => {
    const options = candidates.map((c) => ({
      value: c.id,
      label: `${c.name}${c.badge ? ` · ${c.badge}` : ''}`,
    }));
    if (node !== null && !candidates.some((c) => c.id === node)) {
      const label = nodeName || `#${node}`;
      const searching = searchText !== undefined && searchText !== '';
      if (!searching || label.toLowerCase().includes(searchText.toLowerCase())) {
        options.unshift({ value: node, label });
      }
    }
    return options;
  }, [candidates, node, nodeName, searchText]);

  // The breadcrumb keeps only the recent few steps: it's auxiliary "go back" info, and even a wide screen can't fit a dozen steps,
  // while the **most recent** steps are what you most need to click; earlier ones use "…" to show existence without taking width.
  const MAX_TRAIL = 3;
  const shownTrail = trail.length > MAX_TRAIL ? trail.slice(-MAX_TRAIL) : trail;
  const trailOffset = trail.length - shownTrail.length;

  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
      {/* One row: level-one perspective (dropdown) + level-two object + layout + breadcrumb + actions */}
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
          <Tag>{t('Aggregate perspective has no single object')}</Tag>
        ) : (
          <Select
            showSearch
            style={{ width: 360 }}
            placeholder={loading ? t('Loading candidates…') : t('Select an object')}
            value={node ?? undefined}
            loading={loading}
            onChange={(v: number) => onNodeChange(v)}
            filterOption={false}
            onSearch={onSearch}
            onOpenChange={onOpenChange}
            options={nodeOptions}
          />
        )}
      </Space>

      {/* Breadcrumb: go back to any step. Same row as the filter; when too wide it **clips horizontally** rather than wrapping,
               because wrapping would push the canvas below down entirely. */}
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
          <span style={{ color: 'rgba(0,0,0,0.45)' }}>{t('Back: ')}</span>
          {trailOffset > 0 ? <span style={{ color: 'rgba(0,0,0,0.25)' }}>… ›</span> : null}
          {shownTrail.map((item, i) => {
            const index = trailOffset + i;
            // Clicking the current step "does nothing" (= stays put), so it isn't rendered as a link: no hand cursor,
            // and no hint that it navigates.
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
