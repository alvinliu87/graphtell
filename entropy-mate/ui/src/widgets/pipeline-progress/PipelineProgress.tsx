import { Alert, Card, Progress, Space, Steps, Tag, Tooltip, Typography } from 'antd';
import { InfoCircleOutlined } from '@ant-design/icons';
import { PHASE_HINT, PHASE_LABEL, PHASE_ORDER, type RunStatus } from '@/entities/pipeline';
import { useLocale } from '@/shared/lib/i18n';
import { formatDuration, formatNumber } from '@/shared/lib/format';

/** 建图进度：进行中显示阶段步骤条；完成后压成一行汇总（各阶段明细收进 ⓘ）。 */
export function PipelineProgress({ run, indexing }: { run: RunStatus | null; indexing: boolean }) {
  const currentIndex = run?.current_phase ? PHASE_ORDER.indexOf(run.current_phase) : -1;
  const { t } = useLocale();

  // 完成态：8 个阶段的耗时/计数是调参信息，不是分析结论 —— 常驻 8 行 + 满格进度条
  // 会把"结论与导航"抽屉变成构建日志。压成一行汇总，明细收进 ⓘ tooltip。
  if (!indexing && run && run.phases.length > 0) {
    const phases = run.phases;
    const sum = (f: (p: (typeof phases)[number]) => number) =>
      phases.reduce((s, p) => s + f(p), 0);
    const detail = phases
      .map(
        (r) =>
          `${t(PHASE_LABEL[r.phase] ?? r.phase)} ${formatDuration(r.duration_ms)} · ${t('节点 ')}${formatNumber(r.nodes_created)} · ${t('边 ')}${formatNumber(r.edges_created)}`,
      )
      .join('\n');
    return (
      <Card variant="borderless" style={{ borderRadius: 14 }}>
        <Space size={8} wrap align="center" style={{ fontSize: 12, color: 'rgba(0,0,0,0.45)' }}>
          <Tag color="green" style={{ marginInlineEnd: 0 }}>
            {t('建图完成')}
          </Tag>
          <span>{t('总耗时 ') + formatDuration(sum((p) => p.duration_ms))}</span>
          <span>{t('节点 ') + formatNumber(sum((p) => p.nodes_created))}</span>
          <span>{t('边 ') + formatNumber(sum((p) => p.edges_created))}</span>
          <span>{t('标注 ') + formatNumber(sum((p) => p.annotations_created))}</span>
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
            message={t('正在执行 ') + (run?.current_phase ? t(PHASE_LABEL[run.current_phase] ?? run.current_phase) : t('建图')) + t(' …')}
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
                    {t('节点 ') + formatNumber(r.nodes_created) + t(' · 边 ') + formatNumber(r.edges_created) + t(' · 标注 ') + formatNumber(r.annotations_created) + t(' · 别名 ') + formatNumber(r.aliases_created)}
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
          <Typography.Text type="secondary">{t('暂无运行记录')}</Typography.Text>
        )}
      </Space>
    </Card>
  );
}
