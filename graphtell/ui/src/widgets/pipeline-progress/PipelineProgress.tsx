import { Alert, Card, Progress, Space, Steps, Tag, Tooltip, Typography } from 'antd';
import { InfoCircleOutlined } from '@ant-design/icons';
import { PHASE_HINT, PHASE_LABEL, PHASE_ORDER, type RunStatus } from '@/entities/pipeline';
import { useLocale } from '@/shared/lib/i18n';
import { formatDuration, formatNumber } from '@/shared/lib/format';

/** Build progress: show a phase stepper while running; collapse into one summary line when done (per-phase details folded into ⓘ). */
export function PipelineProgress({ run, indexing }: { run: RunStatus | null; indexing: boolean }) {
  const currentIndex = run?.current_phase ? PHASE_ORDER.indexOf(run.current_phase) : -1;
  const { t } = useLocale();

  // In the done state, the 8 phases' timings / counts are tuning info, not analysis conclusions -- a permanent 8 rows + full-width progress bar
  // would turn the "conclusions and navigation" drawer into a build log. Collapse into one summary line, with details in the ⓘ tooltip.
  if (!indexing && run && run.phases.length > 0) {
    const phases = run.phases;
    const sum = (f: (p: (typeof phases)[number]) => number) =>
      phases.reduce((s, p) => s + f(p), 0);
    const detail = phases
      .map(
        (r) =>
          `${t(PHASE_LABEL[r.phase] ?? r.phase)} ${formatDuration(r.duration_ms)} · ${t('Nodes ')}${formatNumber(r.nodes_created)} · ${t('edges ')}${formatNumber(r.edges_created)}`,
      )
      .join('\n');
    return (
      <Card variant="borderless" style={{ borderRadius: 14 }}>
        <Space size={8} wrap align="center" style={{ fontSize: 12, color: 'rgba(0,0,0,0.45)' }}>
          <Tag color="green" style={{ marginInlineEnd: 0 }}>
            {t('Graphing complete')}
          </Tag>
          <span>{t('Total ') + formatDuration(sum((p) => p.duration_ms))}</span>
          <span>{t('Nodes ') + formatNumber(sum((p) => p.nodes_created))}</span>
          <span>{t('edges ') + formatNumber(sum((p) => p.edges_created))}</span>
          <span>{t('annotations ') + formatNumber(sum((p) => p.annotations_created))}</span>
          <Tooltip title={detail}>
            <InfoCircleOutlined style={{ cursor: 'help', color: 'rgba(0,0,0,0.35)' }} />
          </Tooltip>
        </Space>
      </Card>
    );
  }

  return (
    <Card variant="borderless" style={{ borderRadius: 14 }}>
      <Space direction="vertical" size={16} style={{ width: '100%' }}>
        {indexing ? (
          <Alert
            type="info"
            showIcon
            message={t('Running ') + (run?.current_phase ? t(PHASE_LABEL[run.current_phase] ?? run.current_phase) : t('Build')) + t(' …')}
            description={run?.current_phase ? t(PHASE_HINT[run.current_phase] ?? '') : undefined}
          />
        ) : null}

        <Steps
          size="small"
          current={currentIndex}
          status={indexing ? 'process' : 'finish'}
          items={PHASE_ORDER.map((p) => ({ title: t(PHASE_LABEL[p] ?? p) }))}
        />

        {run && run.phases.length > 0 ? (
          <Space direction="vertical" size={10} style={{ width: '100%' }}>
            {run.phases.map((r) => (
              <div key={r.phase}>
                <Space size={8} style={{ marginBottom: 4 }}>
                  <Typography.Text strong style={{ fontSize: 13 }}>
                    {t(PHASE_LABEL[r.phase] ?? r.phase)}
                  </Typography.Text>
                  <Tag>{formatDuration(r.duration_ms)}</Tag>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    {t('Nodes ') + formatNumber(r.nodes_created) + t(' · edges ') + formatNumber(r.edges_created) + t(' · annotations ') + formatNumber(r.annotations_created) + t(' · aliases ') + formatNumber(r.aliases_created)}
                  </Typography.Text>
                </Space>
                <Progress
                  percent={100}
                  showInfo={false}
                  strokeColor="#3d7eff"
                  size="small"
                  strokeLinecap="butt"
                />
              </div>
            ))}
          </Space>
        ) : (
          <Typography.Text type="secondary">{t('No run records')}</Typography.Text>
        )}
      </Space>
    </Card>
  );
}
