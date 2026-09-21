import { useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Input,
  InputNumber,
  Row,
  Space,
  Spin,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import { CopyOutlined, SearchOutlined } from '@ant-design/icons';
import { useParams } from 'react-router-dom';
import { PageHeader } from '@/shared/ui/PageHeader';
import { recallApi } from '@/entities/recall';
import type { RecallResult } from '@/entities/recall';
import { useLocale } from '@/shared/lib/i18n';

/**
 * 代码召回页：给一段提示词，返回"该看哪些代码"。
 *
 * 设计要点（与后端 `RecallHit.direct` / `hop` 对应）：
 * * **明确区分直接命中与扩展命中** —— 用户必须能看出一条结果为什么在这里，
 *   否则召回与全文检索毫无区别，也无法判断可信度；
 * * **上下文包可一键复制** —— 召回的终点是把结果交给 LLM 或同事，
 *   而不是让人在页面上抄路径；
 * * 纯中文提示词目前只在含标识符或结构提示词（"表"/"接口"/"事件"…）时有效，
 *   页面把解析出的查询词显式展示出来，让用户立刻知道"系统到底搜了什么"。
 */
export function RecallPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();

  const [query, setQuery] = useState('');
  const [limit, setLimit] = useState(20);
  const [hops, setHops] = useState(2);
  const [result, setResult] = useState<RecallResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    const q = query.trim();
    if (!q) {
      message.warning(t('请输入提示词'));
      return;
    }
    setLoading(true);
    setError(null);
    try {
      setResult(await recallApi.recall(id, { query: q, limit, hops, with_snippets: true }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  const copyLocation = async (file?: string | null, line?: number | null) => {
    if (!file) return;
    await navigator.clipboard?.writeText(line ? `${file}:${line}` : file);
    message.success(t('已复制定位'));
  };

  const copyPack = async (md: string) => {
    await navigator.clipboard?.writeText(md);
    message.success(t('上下文包已复制，可直接粘贴给 LLM'));
  };

  return (
    <>
      <PageHeader
        title={t('代码召回')}
        subtitle={t(
          '先按关键词在图上定位种子，再沿调用链把相关的代码一并带出来 —— 命中里会标明哪些是直接命中、哪些是图扩展出来的',
        )}
      />

      <Card variant="borderless" style={{ borderRadius: 14, marginBottom: 16 }}>
        <Space wrap size={10} style={{ width: '100%' }}>
          <Input
            allowClear
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onPressEnter={() => void run()}
            placeholder={t('例如：store_order 订单表 / 用户登录的接口 / obsession 优惠券相关代码')}
            style={{ minWidth: 380, maxWidth: 620 }}
            prefix={<SearchOutlined style={{ color: 'rgba(0,0,0,0.3)' }} />}
          />
          <Tooltip title={t('从种子沿调用链向外扩展几跳')}>
            <InputNumber
              min={0}
              max={4}
              value={hops}
              onChange={(v) => setHops(Number(v ?? 0))}
              addonBefore={t('跳数')}
              style={{ width: 110 }}
            />
          </Tooltip>
          <Tooltip title={t('最多返回多少条')}>
            <InputNumber
              min={1}
              max={100}
              value={limit}
              onChange={(v) => setLimit(Number(v ?? 20))}
              addonBefore={t('条数')}
              style={{ width: 110 }}
            />
          </Tooltip>
          <Button type="primary" loading={loading} onClick={() => void run()}>
            {t('召回')}
          </Button>
          {result ? (
            <Button icon={<CopyOutlined />} onClick={() => void copyPack(result.markdown)}>
              {t('复制上下文包')}
            </Button>
          ) : null}
        </Space>

        {result ? (
          <div style={{ marginTop: 12 }}>
            <Space size={6} wrap>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('解析出的查询词')}：
              </Typography.Text>
              {result.terms.length === 0 ? (
                <Typography.Text type="warning" style={{ fontSize: 12 }}>
                  {t('没有可用于匹配的词 —— 纯中文且不含结构提示时目前无法召回')}
                </Typography.Text>
              ) : (
                result.terms.map((term) => (
                  <Tag key={term} color="blue">
                    {term}
                  </Tag>
                ))
              )}
              {result.kind_hints.map((k) => (
                <Tag key={k} color="purple">
                  {t('结构提示')}：{k}
                </Tag>
              ))}
            </Space>
          </div>
        ) : null}
      </Card>

      {error ? <Alert type="error" showIcon message={error} style={{ marginBottom: 16 }} /> : null}

      {loading ? (
        <div style={{ display: 'grid', placeItems: 'center', padding: 60 }}>
          <Spin />
        </div>
      ) : !result ? (
        <Card variant="borderless" style={{ borderRadius: 14 }}>
          <Empty description={t('输入提示词开始召回')} />
        </Card>
      ) : (
        <Row gutter={[16, 16]}>
          <Col xs={24} lg={8}>
            <Card
              variant="borderless"
              title={t('种子（直接命中）')}
              style={{ borderRadius: 14 }}
              styles={{ body: { paddingTop: 8 } }}
            >
              {result.seeds.length === 0 ? (
                <Empty description={t('没有命中任何种子')} />
              ) : (
                <Space direction="vertical" style={{ width: '100%' }} size={6}>
                  {result.seeds.map((s) => (
                    <div
                      key={s.node_id}
                      style={{
                        display: 'flex',
                        justifyContent: 'space-between',
                        gap: 8,
                        padding: '6px 8px',
                        borderRadius: 8,
                        background: '#f7f8fa',
                      }}
                    >
                      <Tooltip title={`${s.kind} · ${s.name}`}>
                        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>
                          <Tag color="geekblue" style={{ marginInlineEnd: 6 }}>
                            {s.kind}
                          </Tag>
                          {s.name}
                        </span>
                      </Tooltip>
                      <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                        {s.score.toFixed(0)}
                      </Typography.Text>
                    </div>
                  ))}
                </Space>
              )}
            </Card>
          </Col>
          <Col xs={24} lg={16}>
            <Card
              variant="borderless"
              title={`${t('相关代码')} · ${result.hits.length}`}
              style={{ borderRadius: 14 }}
              styles={{ body: { paddingTop: 8 } }}
            >
              {result.hits.length === 0 ? (
                <Empty description={t('没有召回结果')} />
              ) : (
                <Space direction="vertical" style={{ width: '100%' }} size={10}>
                  {result.hits.map((h, i) => (
                    <div
                      key={h.node_id}
                      style={{
                        border: '1px solid #eef0f4',
                        borderRadius: 10,
                        padding: '10px 12px',
                      }}
                    >
                      <div
                        style={{
                          display: 'flex',
                          alignItems: 'center',
                          gap: 8,
                          flexWrap: 'wrap',
                        }}
                      >
                        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                          {i + 1}.
                        </Typography.Text>
                        <Tag color="geekblue">{h.kind}</Tag>
                        <span style={{ fontWeight: 600 }}>{h.name}</span>
                        {h.direct ? (
                          <Tag color="green">{t('直接命中')}</Tag>
                        ) : (
                          <Tooltip title={t('靠图的调用链扩展带出来的')}>
                            <Tag color="default">
                              {t('扩展')} · {t('跳数')} {h.hop}
                            </Tag>
                          </Tooltip>
                        )}
                        <span style={{ marginInlineStart: 'auto', fontSize: 12 }}>
                          <Typography.Text type="secondary">
                            {h.score.toFixed(1)}
                          </Typography.Text>
                        </span>
                      </div>

                      <div
                        style={{
                          marginTop: 6,
                          display: 'flex',
                          gap: 10,
                          flexWrap: 'wrap',
                          fontSize: 12,
                        }}
                      >
                        <Button
                          type="link"
                          size="small"
                          icon={<CopyOutlined />}
                          style={{ paddingInline: 0, fontSize: 12 }}
                          onClick={() => void copyLocation(h.file, h.line)}
                        >
                          {h.file
                            ? `${h.file.split('/').pop()}${h.line ? `:${h.line}` : ''}`
                            : t('无位置信息')}
                        </Button>
                        {h.relations.map((r) => (
                          <Tag key={r} bordered={false} style={{ margin: 0 }}>
                            {r}
                          </Tag>
                        ))}
                        {!h.direct ? (
                          <Typography.Text type="secondary">
                            ← {t('来自种子')} <b>{h.seed}</b>
                          </Typography.Text>
                        ) : null}
                      </div>

                      {h.snippet ? (
                        <pre
                          style={{
                            margin: '8px 0 0',
                            padding: 10,
                            background: '#f7f8fa',
                            borderRadius: 8,
                            fontSize: 12,
                            lineHeight: 1.55,
                            overflowX: 'auto',
                          }}
                        >
                          {h.snippet}
                        </pre>
                      ) : null}
                    </div>
                  ))}
                </Space>
              )}
            </Card>
          </Col>
        </Row>
      )}
    </>
  );
}
