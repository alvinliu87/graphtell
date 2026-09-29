import { useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Input,
  InputNumber,
  Popover,
  Row,
  Space,
  Spin,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import { CopyOutlined } from '@ant-design/icons';
import { useParams } from 'react-router-dom';
import { PageHeader } from '@/shared/ui/PageHeader';
import { recallApi } from '@/entities/recall';
import type { ComposePromptResult, RecallResult } from '@/entities/recall';
import { useLocale } from '@/shared/lib/i18n';

/**
 * 召回质量条：把服务端判定的质量档位显式呈现出来。
 *
 * 为什么必须有：召回质量**方差极大** —— 有的查询正解在前二，有的两个意图都落空、
 * 前排全是泛词噪声，但两者返回的列表长得一模一样。不标出来的话用户会同等信任，
 * 于是「静默失败」成了最坏的失败模式。
 *
 * 非「高」时把未命中的特征词渲染成可点击 chip：点一下即用该词重新召回，
 * 给用户一条明确的退路，而不是只告诉他"这次不准"。
 */
function RecallQualityBanner({
  result,
  onPickTerm,
}: {
  result: RecallResult;
  onPickTerm: (kw: string) => void;
}) {
  const { t } = useLocale();
  const meta =
    result.quality === 'low'
      ? { color: 'red', label: t('低'), type: 'error' as const }
      : result.quality === 'medium'
        ? { color: 'orange', label: t('中'), type: 'warning' as const }
        : { color: 'green', label: t('高'), type: 'success' as const };

  return (
    <Alert
      type={meta.type}
      showIcon
      style={{ marginBottom: 10 }}
      message={
        <Space size={6} wrap>
          <Typography.Text strong>{t('召回质量')}</Typography.Text>
          <Tag color={meta.color}>{meta.label}</Tag>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {t('置信度')} {(result.confidence ?? 1).toFixed(2)}
          </Typography.Text>
        </Space>
      }
      description={
        <>
          <div style={{ fontSize: 12 }}>{result.quality_reason}</div>
          {result.missing_terms?.length ? (
            <div style={{ marginTop: 6 }}>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('建议改用这些特征词检索')}：
              </Typography.Text>
              {result.missing_terms.map((m) => (
                <Tag
                  key={m}
                  color={meta.color}
                  style={{ cursor: 'pointer' }}
                  onClick={() => onPickTerm(m)}
                >
                  {m}
                </Tag>
              ))}
            </div>
          ) : null}
        </>
      }
    />
  );
}

/**
 * 提示词增强页：给一段提示词，返回"该看哪些代码"，并合成成可直接粘给 IDE 的提示词。
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
  const [result, setResult] = useState<ComposePromptResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 合成后的提示词全文默认展开：它就是这一页的产物，藏起来等于白做。 */
  const [showPrompt, setShowPrompt] = useState(true);

  // `override`：供质量条上的特征词 chip 直接以该词重新召回 ——
  // 不能只 setQuery 再 run()，因为 setState 是异步的，run 会读到旧值。
  const run = async (override?: string) => {
    const q = (override ?? query).trim();
    if (!q) {
      message.warning(t('先描述你要找什么'));
      return;
    }
    setLoading(true);
    setError(null);
    try {
      // 走合成接口而不是纯召回：这一页要交付的是"增强后的提示词"，
      // 命中列表只是让用户核对召回质量的伴随结果（后端一次返回，召回不会跑两遍）。
      setResult(
        await recallApi.compose(id, {
          // 同一份：这段话既是召回用的检索词，也是写进【本次任务】的任务描述。
          query: q,
          intent: q,
          limit,
          hops,
          with_snippets: true,
        }),
      );
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

  const copyPrompt = async (p: string) => {
    await navigator.clipboard?.writeText(p);
    message.success(t('提示词已复制，可直接粘贴给 IDE'));
  };

  return (
    <>
      <PageHeader
        title={t('提示词增强')}
        subtitle={t(
          '把你本来要发给 IDE 的那段话写在这里，系统按图召回相关代码并合成进去 —— 这一页不调用大模型，只做检索与拼装',
        )}
      />

      {/*
        提问区：**一个**输入框，不是"检索词 + 任务"两个。

        为什么合成一个：这页交付的是"把你写的提示词增强一下"，用户写的那段话
        既是检索词（拿去召回）也是任务（原样写进提示词的【本次任务】）。
        拆成两栏会让它读起来像填表单，也与页面名不符。

        后端仍保留 `query` / `intent` 两个字段：真出现"用 A 检索、让 LLM 做 B"的场景时，
        加一个折叠项即可，不必动后端。现阶段 `intent` 与 `query` 同一份。

        多行是必要的（提示词本来就是一段话），因此提交改为 ⌘/Ctrl+Enter ——
        回车要留给换行，否则写两行就跑了。

        跳数 / 条数收进折叠：它们是"调一次就不动"的参数，与输入框平排会把整行读成筛选栏。
      */}
      <Card variant="borderless" style={{ borderRadius: 14, marginBottom: 16 }}>
        <Input.TextArea
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) void run();
          }}
          autoSize={{ minRows: 3, maxRows: 8 }}
          placeholder={t('你要找什么 / 想让 IDE 帮你做什么 —— 这段会原样写进提示词的【本次任务】')}
          style={{ maxWidth: 720 }}
        />
        <Space align="center" style={{ marginTop: 10 }}>
          <Button type="primary" loading={loading} onClick={() => void run()}>
            {t('生成提示词')}
          </Button>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {t('⌘/Ctrl + Enter 直接生成')}
          </Typography.Text>
        </Space>

        <Popover
          trigger="click"
          placement="bottomLeft"
          content={
            <Space direction="vertical" size={10}>
              <Tooltip title={t('从种子沿调用链向外扩展几跳')}>
                <InputNumber
                  min={0}
                  max={4}
                  value={hops}
                  onChange={(v) => setHops(Number(v ?? 0))}
                  addonBefore={t('跳数')}
                  style={{ width: 140 }}
                />
              </Tooltip>
              <Tooltip title={t('最多返回多少条')}>
                <InputNumber
                  min={1}
                  max={100}
                  value={limit}
                  onChange={(v) => setLimit(Number(v ?? 20))}
                  addonBefore={t('条数')}
                  style={{ width: 140 }}
                />
              </Tooltip>
            </Space>
          }
        >
          {/* 折叠但**不隐藏**：当前值始终以一行小字显示，用户知道这里有旋钮 */}
          <Button type="text" size="small" style={{ paddingInline: 0, marginTop: 4, fontSize: 12 }}>
            {t('参数')}：{t('跳数')} {hops} · {t('条数')} {limit} ▸
          </Button>
        </Popover>

        {/* loading 时隐藏上一次的结果摘要与质量条：否则会残留旧内容，
            与下方转圈的 loading 区同时出现，看起来像"新结果已经出来了"。 */}
        {result && !loading ? (
          <div style={{ marginTop: 12 }}>
            <RecallQualityBanner
              result={result.recall}
              onPickTerm={(kw) => {
                setQuery(kw);
                void run(kw);
              }}
            />
            <Space size={6} wrap>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('解析出的查询词')}：
              </Typography.Text>
              {result.recall.terms.length === 0 ? (
                <Typography.Text type="warning" style={{ fontSize: 12 }}>
                  {t('没有可用于匹配的词 —— 纯中文且不含结构提示时目前无法召回')}
                </Typography.Text>
              ) : (
                result.recall.terms.map((term) => (
                  <Tag key={term} color="blue">
                    {term}
                  </Tag>
                ))
              )}
              {result.recall.kind_hints.map((k) => (
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
        /*
          空态只留两句说明，不放示例：示例要么写死某个工程的表名（换个工程就文不对题），
          要么空泛到没信息。而输入框的 placeholder 与页头副标题已经说明了"写什么"。
          旧版这里是一句灰字"输入提示词开始召回"，什么都不教 —— 现在至少有这两句。
        */
        <Card variant="borderless" style={{ borderRadius: 14 }}>
          <Empty
            description={
              <div style={{ fontSize: 13 }}>
                <div>{t('用一句话描述你要找的代码')}</div>
                <div style={{ marginTop: 4, color: 'rgba(0,0,0,0.45)' }}>
                  {t('系统会按图召回相关代码，并合成成一段可直接粘给 IDE 的提示词')}
                </div>
              </div>
            }
          />
        </Card>
      ) : (
        <>
          {/*
            合成后的提示词 —— 这一页的产物，所以放在结果最上面。
            两个复制按钮并存是有意的：提示词（任务 + 上下文，直接用）与
            上下文包（只有后半段，自己剪/自己拼）是两种用法，不是重复功能。
          */}
          <Card
            variant="borderless"
            style={{ borderRadius: 14, marginBottom: 16 }}
            title={
              <Space size={8} wrap>
                <span>{t('增强后的提示词')}</span>
                <Tag color="geekblue">≈ {result.approx_tokens} tokens</Tag>
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('命中')} {result.hit_count}{t(' 条')} · {t('种子')} {result.seed_count}
                </Typography.Text>
              </Space>
            }
            extra={
              <Space>
                <Button type="link" onClick={() => setShowPrompt((v) => !v)}>
                  {showPrompt ? t('收起') : t('展开全文')}
                </Button>
                <Button icon={<CopyOutlined />} onClick={() => void copyPrompt(result.prompt)}>
                  {t('复制提示词')}
                </Button>
                <Button icon={<CopyOutlined />} onClick={() => void copyPack(result.markdown)}>
                  {t('复制上下文包')}
                </Button>
              </Space>
            }
          >
            {showPrompt ? (
              <pre
                style={{
                  margin: 0,
                  maxHeight: 420,
                  overflow: 'auto',
                  padding: 12,
                  background: '#f7f8fa',
                  borderRadius: 8,
                  fontSize: 12,
                  lineHeight: 1.6,
                  whiteSpace: 'pre-wrap',
                }}
              >
                {result.prompt}
              </pre>
            ) : null}
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {t('提示词 = 你的任务 + 按图召回的代码上下文 + 质量约束；上下文包只有中间那段，可自行裁剪')}
            </Typography.Text>
          </Card>

          <Row gutter={[16, 16]}>
          <Col xs={24} lg={8}>
            <Card
              variant="borderless"
              title={t('种子（直接命中）')}
              style={{ borderRadius: 14 }}
              styles={{ body: { paddingTop: 8 } }}
            >
              {result.recall.seeds.length === 0 ? (
                <Empty description={t('没有命中任何种子')} />
              ) : (
                <Space direction="vertical" style={{ width: '100%' }} size={6}>
                  {result.recall.seeds.map((s) => (
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
              title={`${t('相关代码')} · ${result.recall.hits.length}`}
              style={{ borderRadius: 14 }}
              styles={{ body: { paddingTop: 8 } }}
            >
              {result.recall.hits.length === 0 ? (
                <Empty description={t('没有召回结果')} />
              ) : (
                <Space direction="vertical" style={{ width: '100%' }} size={10}>
                  {result.recall.hits.map((h, i) => (
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
        </>
      )}
    </>
  );
}
