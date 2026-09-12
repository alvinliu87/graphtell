import { Alert, Card, Progress, Space, Steps, Tag, Typography } from 'antd';
import { PHASE_HINT, PHASE_LABEL, PHASE_ORDER, type RunStatus } from '@/entities/pipeline';
import { formatDuration, formatNumber } from '@/shared/lib/format';

/** 建图进度：阶段步骤条 + 每阶段产物统计。 */
export function PipelineProgress({ run, indexing }: { run: RunStatus | null; indexing: boolean }) {
  const currentIndex = run?.current_phase ? PHASE_ORDER.indexOf(run.current_phase) : -1;

  return (
    <Card variant="borderless" style={{ borderRadius: 14 }}>
      <Space direction="vertical" size={16} style={{ width: '100%' }}>
        {indexing ? (
          <Alert
            type="info"
            showIcon
            message={`正在执行 ${run?.current_phase ? PHASE_LABEL[run.current_phase] ?? run.current_phase : '建图'} …`}
            description={run?.current_phase ? PHASE_HINT[run.current_phase] : undefined}
          />
        ) : null}

        <Steps
          size="small"
          current={currentIndex}
          status={indexing ? 'process' : 'finish'}
          items={PHASE_ORDER.map((p) => ({ title: PHASE_LABEL[p] ?? p }))}
        />

        {run && run.phases.length > 0 ? (
          <Space direction="vertical" size={10} style={{ width: '100%' }}>
            {run.phases.map((r) => (
              <div key={r.phase}>
                <Space size={8} style={{ marginBottom: 4 }}>
                  <Typography.Text strong style={{ fontSize: 13 }}>
                    {PHASE_LABEL[r.phase] ?? r.phase}
                  </Typography.Text>
                  <Tag>{formatDuration(r.duration_ms)}</Tag>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    节点 {formatNumber(r.nodes_created)} · 边 {formatNumber(r.edges_created)} · 标注{' '}
                    {formatNumber(r.annotations_created)} · 别名 {formatNumber(r.aliases_created)}
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
          <Typography.Text type="secondary">暂无运行记录</Typography.Text>
        )}
      </Space>
    </Card>
  );
}
