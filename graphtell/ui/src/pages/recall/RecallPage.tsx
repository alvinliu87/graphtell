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
 * Recall quality bar: surfaces the quality tier the server decided.
 *
 * Why it must exist: recall quality **varies enormously** -- some queries have the right answer in the top two, others miss both intents and
 * the front is all generic-word noise, yet both return an identical-looking list. Without marking it, users trust them equally,
 * so "silent failure" becomes the worst failure mode.
 *
 * When not "high", render the missed feature words as clickable chips: clicking reruns recall with that word,
 * giving the user a clear way out instead of only telling them "this one is inaccurate".
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
      ? { color: 'red', label: t('Low'), type: 'error' as const }
      : result.quality === 'medium'
        ? { color: 'orange', label: t('Medium'), type: 'warning' as const }
        : { color: 'green', label: t('High'), type: 'success' as const };

  return (
    <Alert
      type={meta.type}
      showIcon
      style={{ marginBottom: 10 }}
      message={
        <Space size={6} wrap>
          <Typography.Text strong>{t('Recall quality')}</Typography.Text>
          <Tag color={meta.color}>{meta.label}</Tag>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {t('Confidence')} {(result.confidence ?? 1).toFixed(2)}
          </Typography.Text>
        </Space>
      }
      description={
        <>
          <div style={{ fontSize: 12 }}>{result.quality_reason}</div>
          {result.missing_terms?.length ? (
            <div style={{ marginTop: 6 }}>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('Try searching with these feature words instead')}：
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
 * Prompt-augmentation page: given a prompt, return "which code to look at", and compose a prompt you can paste straight into an IDE.
 *
 * Design notes (matching the backend's `RecallHit.direct` / `hop`):
 * * **clearly separate direct hits from expanded hits** -- the user must be able to see why a result is here,
 *   otherwise recall is no different from full-text search and trust can't be judged;
 * * **the context pack is one-click copyable** -- recall's endpoint is handing the result to an LLM or a colleague,
 *   not making people copy paths off the page;
 * * a pure-Chinese prompt currently only works when it contains an identifier or a structural hint word ("table" / "interface" / "event" …);
 *   the page shows the parsed query terms explicitly so the user immediately knows "what the system actually searched for".
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
  /** The composed prompt is expanded by default: it is this page's deliverable, so hiding it defeats the purpose. */
  const [showPrompt, setShowPrompt] = useState(true);

  // `override`: lets the feature-word chips on the quality bar rerun recall with that word directly --
  // can't just setQuery then run(), because setState is async and run would read the old value.
  const run = async (override?: string) => {
    const q = (override ?? query).trim();
    if (!q) {
      message.warning(t('Describe what you are looking for first'));
      return;
    }
    setLoading(true);
    setError(null);
    try {
      // Use the compose endpoint rather than pure recall: what this page delivers is "the augmented prompt";
      // the hit list is only an accompanying result for checking recall quality (the backend returns it in one go, so recall doesn't run twice).
      setResult(
        await recallApi.compose(id, {
          // Same text: this paragraph is both the search terms for recall and the task description written into [this task].
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
    message.success(t('Location copied'));
  };

  const copyPack = async (md: string) => {
    await navigator.clipboard?.writeText(md);
    message.success(t('Context pack copied — paste it straight into your LLM'));
  };

  const copyPrompt = async (p: string) => {
    await navigator.clipboard?.writeText(p);
    message.success(t('Prompt copied — paste it into your IDE'));
  };

  return (
    <>
      <PageHeader
        title={t('Prompt augmentation')}
        subtitle={t('Paste the message you would send to your IDE; the system recalls related code from the graph and composes it in — this page calls no LLM, it only retrieves and assembles.')}
      />

      {/*
        Question area: **one** input box, not two ("search terms + task").

        Why compose into one: this page delivers "augment the prompt you wrote"; the paragraph the user writes
        is both the search terms (sent to recall) and the task (written verbatim into the prompt's [this task]).
        Splitting it into two columns would read like filling out a form, and wouldn't match the page name.

        The backend still keeps both `query` / `intent` fields: if a real "search with A, have the LLM do B" case appears,
        add a collapsible item -- no backend change needed. For now `intent` and `query` are the same text.

        Multi-line is necessary (a prompt is a paragraph by nature), so submit is ⌘/Ctrl+Enter --
        Enter must stay for newlines, otherwise two lines and it fires.

        Hop count / result count go into the collapse: they are "set once and forget" params, and sitting in a row with the input makes the whole line read as a filter bar.
      */}
      <Card variant="borderless" style={{ borderRadius: 14, marginBottom: 16 }}>
        <Input.TextArea
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) void run();
          }}
          autoSize={{ minRows: 3, maxRows: 8 }}
          placeholder={t('What are you looking for / what should the IDE do — this is copied verbatim into the prompt’s TASK section')}
          style={{ maxWidth: 720 }}
        />
        <Space align="center" style={{ marginTop: 10 }}>
          <Button type="primary" loading={loading} onClick={() => void run()}>
            {t('Compose prompt')}
          </Button>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {t('⌘/Ctrl + Enter to generate')}
          </Typography.Text>
        </Space>

        <Popover
          trigger="click"
          placement="bottomLeft"
          content={
            <Space direction="vertical" size={10}>
              <Tooltip title={t('How many hops to expand outward from the seeds along the call chain')}>
                <InputNumber
                  min={0}
                  max={4}
                  value={hops}
                  onChange={(v) => setHops(Number(v ?? 0))}
                  addonBefore={t('Hops')}
                  style={{ width: 140 }}
                />
              </Tooltip>
              <Tooltip title={t('Maximum number of results to return')}>
                <InputNumber
                  min={1}
                  max={100}
                  value={limit}
                  onChange={(v) => setLimit(Number(v ?? 20))}
                  addonBefore={t('Count')}
                  style={{ width: 140 }}
                />
              </Tooltip>
            </Space>
          }
        >
          {/* Collapsed but **not hidden**: the current value always shows as a line of small text, so the user knows the knob is here */}
          <Button type="text" size="small" style={{ paddingInline: 0, marginTop: 4, fontSize: 12 }}>
            {t('Options')}：{t('Hops')} {hops} · {t('Count')} {limit} ▸
          </Button>
        </Popover>

        {/* While loading, hide the previous result summary and quality bar: otherwise stale content lingers and
                 appears together with the spinning loading area below, looking like "the new results are already out". */}
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
                {t('Parsed query terms')}：
              </Typography.Text>
              {result.recall.terms.length === 0 ? (
                <Typography.Text type="warning" style={{ fontSize: 12 }}>
                  {t('No terms to match against — pure Chinese without structural hints cannot be recalled yet')}
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
                  {t('Structural hints')}：{k}
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
          The empty state keeps only two sentences, no examples: an example either hardcodes some project's table names (wrong for another project)
          or is vague enough to carry no information. The input placeholder and the page subtitle already say "what to write".
          The old version had one gray line "enter a prompt to start recall", teaching nothing -- now there are at least these two sentences.
        */
        <Card variant="borderless" style={{ borderRadius: 14 }}>
          <Empty
            description={
              <div style={{ fontSize: 13 }}>
                <div>{t('Describe the code you are looking for in one sentence')}</div>
                <div style={{ marginTop: 4, color: 'rgba(0,0,0,0.45)' }}>
                  {t('The system recalls related code from the graph and composes a prompt you can paste into your IDE.')}
                </div>
              </div>
            }
          />
        </Card>
      ) : (
        <>
          {/*
            The composed prompt -- this page's deliverable, so it goes at the top of the results.
            Two copy buttons coexist on purpose: the prompt (task + context, use directly) and
            the context pack (only the latter half, trim/splice yourself) are two usages, not a duplicated feature.
          */}
          <Card
            variant="borderless"
            style={{ borderRadius: 14, marginBottom: 16 }}
            title={
              <Space size={8} wrap>
                <span>{t('Augmented prompt')}</span>
                <Tag color="geekblue">≈ {result.approx_tokens} tokens</Tag>
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('Hits')} {result.hit_count}{t(' entries')} · {t('seeds')} {result.seed_count}
                </Typography.Text>
              </Space>
            }
            extra={
              <Space>
                <Button type="link" onClick={() => setShowPrompt((v) => !v)}>
                  {showPrompt ? t('Collapse') : t('Show full text')}
                </Button>
                <Button icon={<CopyOutlined />} onClick={() => void copyPrompt(result.prompt)}>
                  {t('Copy prompt')}
                </Button>
                <Button icon={<CopyOutlined />} onClick={() => void copyPack(result.markdown)}>
                  {t('Copy context pack')}
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
              {t('Prompt = your task + graph-recalled code context + quality constraints; the context pack is only the middle part, trim it as you like.')}
            </Typography.Text>
          </Card>

          <Row gutter={[16, 16]}>
          <Col xs={24} lg={8}>
            <Card
              variant="borderless"
              title={t('Seeds (direct hits)')}
              style={{ borderRadius: 14 }}
              styles={{ body: { paddingTop: 8 } }}
            >
              {result.recall.seeds.length === 0 ? (
                <Empty description={t('No seed matched')} />
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
              title={`${t('Related code')} · ${result.recall.hits.length}`}
              style={{ borderRadius: 14 }}
              styles={{ body: { paddingTop: 8 } }}
            >
              {result.recall.hits.length === 0 ? (
                <Empty description={t('No recall results')} />
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
                          <Tag color="green">{t('Direct hit')}</Tag>
                        ) : (
                          <Tooltip title={t('Pulled in by call-chain expansion on the graph')}>
                            <Tag color="default">
                              {t('Expanded')} · {t('Hops')} {h.hop}
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
                            : t('No location')}
                        </Button>
                        {h.relations.map((r) => (
                          <Tag key={r} bordered={false} style={{ margin: 0 }}>
                            {r}
                          </Tag>
                        ))}
                        {!h.direct ? (
                          <Typography.Text type="secondary">
                            ← {t('From a seed')} <b>{h.seed}</b>
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
