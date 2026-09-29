import { useMemo, useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Row,
  Segmented,
  Select,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import { CopyOutlined } from '@ant-design/icons';
import { useNavigate, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import {
  CATEGORY_COLOR,
  actionableCount,
  graphApi,
  groupDiagnostics,
  type Diagnostic,
  type DiagnosticCategory,
  type DiagnosticGroup,
  type DiagnosticSummary,
} from '@/entities/graph';
import {
  SEVERITY_COLOR,
  SEVERITY_LABEL,
  SEVERITY_ORDER,
  SEVERITY_RANK,
  checkApi,
  type Severity,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';

/** 统计卡强调色（`SEVERITY_COLOR` 是 antd 语义色名，给 Tag 用，两者用途不同）。 */
const SEVERITY_ACCENT: Record<Severity, string> = {
  critical: '#a8071a',
  error: '#ff4d4f',
  warning: '#fa8c16',
  info: '#3d7eff',
};

/**
 * 分类徽章：三类的处置完全不同，所以徽章必须显式 —— 用户读到「349 条」时的第一反应是
 * "我是不是有 349 个 bug"，而真相往往是"1 类引擎局限发生了 349 次"。
 */
const CATEGORY_TEXT: Record<DiagnosticCategory, { label: string; hint: string }> = {
  actionable: {
    label: '值得看一眼',
    hint: '可能指向真实的代码问题（死路由 / 事件没注册），值得核对。',
  },
  engine: {
    label: '引擎 / 知识局限',
    hint: '引擎或框架知识没能建出这一块：代码本身没问题，但图在这里是缺的，相关召回会弱。',
  },
  expected: {
    label: '预期内',
    hint: '目标在 vendor 或被排除，属设计如此，不用管。',
  },
};

/** 每类问题最多直接列出的样例位置数（其余计数而已，不宜把页面铺成一堵墙）。 */
const SAMPLE_LIMIT = 3;

/**
 * 建图报告页：**建图期**诊断（冲突、缺失、未解析链接）—— 这些本身就是有价值的发现。
 *
 * 两条设计主线（都是踩过坑才有的）：
 *
 * 1. **规则检验的结论刻意不在这里**：两者虽然都存在诊断表里，但性质不同
 *    （"图没建全" vs "代码违反了规则"），混在一张表里只会让两类结论都读不懂。
 *    这里给一个显式入口指过去，避免用户以为"一条错误都没有"。
 * 2. **按问题类型分组，而不是平铺条目**：诊断天生长尾重复（同一条引擎诊断在几百个文件上
 *    各触发一次），平铺的表格会把"同一件事发生 349 次"呈现成"349 个问题"，
 *    且 `ORDER BY id DESC` 的截断让用户看到的是写入顺序、不是严重程度。
 *    分组 + 分类 + 人话说明之后，"要不要管"才是读得出来的。
 */
export function CoveragePage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();
  const navigate = useNavigate();

  const list = useAsync<Diagnostic[]>(() => graphApi.diagnostics(id), [id]);
  const summary = useAsync<DiagnosticSummary>(() => graphApi.diagnosticsSummary(id), [id]);
  const check = useAsync(() => checkApi.summary(id), [id]);

  /** 视图：按类型归类的卡片 / 逐条明细表。默认归类 —— 它才是"读得懂"的那一层。 */
  const [view, setView] = useState<'types' | 'list'>('types');
  const [severity, setSeverity] = useState<Severity | 'all'>('all');
  const [code, setCode] = useState<string | 'all'>('all');

  const s = summary.data;
  const total = s ? s.critical + s.error + s.warning + s.info : 0;
  const listed = list.data?.length ?? 0;

  const groups = useMemo(
    () => groupDiagnostics(summary.data?.by_code, list.data ?? []),
    [summary.data, list.data],
  );
  const actionable = actionableCount(groups);
  const byCodeAccurate = (summary.data?.by_code?.length ?? 0) > 0;

  /** 明细表：严重度优先 → 类型，与「按类型」视图同一份严重度权重。 */
  const rows = useMemo(
    () =>
      (list.data ?? [])
        .filter((d) => severity === 'all' || d.severity === severity)
        .filter((d) => code === 'all' || d.code === code)
        .slice()
        .sort(
          (a, b) => SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] || a.code.localeCompare(b.code),
        ),
    [list.data, severity, code],
  );

  /** 通俗名：词条未收录的 code 原样显示，绝不猜含义（`t` 未命中时回退为键本身）。 */
  const codeTitle = (c: string) => {
    const key = `diag.${c}.title`;
    const v = t(key);
    return v === key ? c : v;
  };
  const codeWhat = (c: string) => {
    const key = `diag.${c}.what`;
    const v = t(key);
    return v === key ? '' : v;
  };

  const copyLocation = (loc: string | null | undefined) => {
    if (!loc) return;
    void navigator.clipboard?.writeText(loc);
    message.success(t('已复制定位'));
  };

  const focusOn = (c: string) => {
    setCode(c);
    setSeverity('all');
    setView('list');
  };

  /** 类的严重度分布，如「警告 349 · 提示 65」：同一 code 在不同严重度下含义可能不同。 */
  const severitySplit = (g: DiagnosticGroup) =>
    SEVERITY_ORDER.filter((sev) => (g.bySeverity[sev] ?? 0) > 0)
      .map((sev) => `${t(SEVERITY_LABEL[sev])} ${g.bySeverity[sev]}`)
      .join(' · ');

  const checkTotal = check.data
    ? check.data.critical + check.data.error + check.data.warning + check.data.info
    : 0;
  const unsupported = s?.unsupported_languages ?? [];

  return (
    <>
      <PageHeader
        title={t('建图报告')}
        subtitle={t('根节点缺失、路由指向不存在的 handler、identity 冲突等 —— 这些记录本身就是分析结论')}
        // 侧栏不再有「诊断」菜单项：这一页的唯一入口是代码图标题旁 ⓘ 的 Popover，
        // 所以这里必须给出回路，否则用户进来就出不去了（只能靠浏览器后退）。
        extra={
          <Button size="small" onClick={() => navigate(`/projects/${id}/graph`)}>
            {t('回到代码图')}
          </Button>
        }
      />

      <Alert
        type={s && s.error + s.critical > 0 ? 'warning' : 'info'}
        showIcon
        style={{ marginBottom: 12 }}
        message={t('这一页是「建图报告」：记录图没建全的地方，不是你的代码违反了规则')}
        description={
          <div>
            <div>
              {t('根节点没解析、路由指向不存在的 handler、identity 算不出来…… 说的都是「图少建了一块」。')}
            </div>
            <div style={{ marginTop: 4 }}>
              {t('读法：先看「问题类型」有几类、哪类要管；同一类在几百个文件上重复触发时，条数不代表问题数。')}
            </div>
            {checkTotal > 0 ? (
              <div style={{ marginTop: 8 }}>
                <Space size={8} wrap>
                  <span>
                    {t('代码是否违反规则见')}
                    {t('规则检验')}：{t('共 ')}
                    {checkTotal}
                    {t(' 条')}（
                    {SEVERITY_ORDER.map((sev, i) => (
                      <span key={sev}>
                        {i > 0 ? ' · ' : ''}
                        {t(SEVERITY_LABEL[sev])} {check.data?.[sev] ?? 0}
                      </span>
                    ))}
                    ）
                  </span>
                  <Button size="small" onClick={() => navigate(`/projects/${id}/check`)}>
                    {t('查看规则检验')}
                  </Button>
                </Space>
              </div>
            ) : null}
          </div>
        }
      />

      {unsupported.length > 0 ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 12 }}
          message={t('这些语言还没有解析器：') + unsupported.join(' · ')}
          description={t(' —— 对应子工程只有文件结构，语义召回在这里为空。')}
        />
      ) : null}

      <Row gutter={[16, 16]} style={{ marginBottom: 16 }}>
        <Col xs={12} md={8}>
          <StatCard
            title={t('问题类型')}
            value={groups.length}
            accent="#7c5cff"
            suffix={groups.length ? t(' 类') : undefined}
          />
        </Col>
        {SEVERITY_ORDER.map((sev) => (
          <Col key={sev} xs={12} md={4}>
            <StatCard
              title={t(SEVERITY_LABEL[sev])}
              value={s?.[sev] ?? 0}
              accent={SEVERITY_ACCENT[sev]}
            />
          </Col>
        ))}
      </Row>

      {total > 0 ? (
        <Alert
          type={actionable > 0 ? 'warning' : 'success'}
          showIcon
          style={{ marginBottom: 12 }}
          message={
            <span>
              {groups.length} {t(' 类问题')}
              {actionable > 0 ? ` · ${t('值得看一眼')} ${actionable} ${t(' 条')}` : ''}
            </span>
          }
          description={
            actionable > 0
              ? t('其余是引擎 / 知识局限与预期内：不改变召回结论，除非你要查的正是那一块。')
              : t('这一页没有需要你处理的：全部是预期内 / 引擎局限。')
          }
        />
      ) : null}

      {total === 0 && !summary.loading && !summary.error ? (
        <Card variant="borderless" style={{ borderRadius: 14 }}>
          <Empty description={t('暂无建图报告 —— 图没报出任何未解析 / 缺失。')} />
        </Card>
      ) : null}

      {total > 0 ? (
        <Card
          variant="borderless"
          style={{ borderRadius: 14 }}
          title={
            <Segmented
              size="small"
              value={view}
              onChange={(v) => setView(v as 'types' | 'list')}
              options={[
                { label: `${t('按问题类型')}（${groups.length}）`, value: 'types' },
                { label: `${t('逐条明细')}（${listed}）`, value: 'list' },
              ]}
            />
          }
          extra={
            byCodeAccurate ? (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('按类型的计数取自全量汇总，不受明细读取上限影响')}
              </Typography.Text>
            ) : (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('按类型的计数取自当前读取窗口（汇总接口未返回按类型计数），可能偏小。')}
              </Typography.Text>
            )
          }
        >
          {view === 'types' ? (
            groups.length === 0 ? (
              <Empty description={t('暂无建图报告条目')} />
            ) : (
              <>
                {groups.map((g) => {
                  const cat = CATEGORY_TEXT[g.category];
                  const samples = g.samples.slice(0, SAMPLE_LIMIT);
                  return (
                    <Card
                      key={g.code}
                      size="small"
                      style={{ borderRadius: 12, marginBottom: 10, background: '#fcfcfd' }}
                      styles={{ body: { padding: 16 } }}
                    >
                      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                        <Tag color={SEVERITY_COLOR[g.severity]} style={{ marginInlineEnd: 0 }}>
                          {t(SEVERITY_LABEL[g.severity])}
                        </Tag>
                        <Tooltip title={t(cat.hint)}>
                          <Tag color={CATEGORY_COLOR[g.category]} style={{ marginInlineEnd: 0 }}>
                            {t(cat.label)}
                          </Tag>
                        </Tooltip>
                        <Typography.Text strong style={{ fontSize: 15 }}>
                          {codeTitle(g.code)}
                        </Typography.Text>
                        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                          <code>{g.code}</code>
                        </Typography.Text>
                        <span style={{ marginLeft: 'auto', fontSize: 12, color: 'rgba(0,0,0,0.55)' }}>
                          {t('这一类共 ')}
                          <b>{g.count}</b>
                          {t(' 条')}
                          {severitySplit(g) && severitySplit(g) !== `${t(SEVERITY_LABEL[g.severity])} ${g.count}`
                            ? `（${severitySplit(g)}）`
                            : ''}
                        </span>
                        <Button size="small" type="link" onClick={() => focusOn(g.code)}>
                          {t('查看明细')}
                        </Button>
                      </div>

                      {codeWhat(g.code) ? (
                        <Typography.Paragraph
                          type="secondary"
                          style={{ margin: '8px 0 0', fontSize: 13 }}
                        >
                          {codeWhat(g.code)}
                        </Typography.Paragraph>
                      ) : null}

                      {samples.length > 0 ? (
                        <div style={{ marginTop: 6 }}>
                          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                            {t('样例位置')}
                          </Typography.Text>
                          <ul style={{ margin: '2px 0 0', paddingLeft: 18 }}>
                            {samples.map((d, i) => (
                              <li key={`${d.location ?? ''}-${i}`}>
                                <Tooltip title={d.message}>
                                  <Button
                                    type="link"
                                    size="small"
                                    icon={<CopyOutlined />}
                                    onClick={() => copyLocation(d.location)}
                                    style={{ paddingInline: 0, fontSize: 12, height: 'auto', maxWidth: '100%' }}
                                  >
                                    {/* 位置可能是整条绝对路径（如框架根目录诊断），必须自己截断 —— 否则会把卡片撑破 */}
                                    <span
                                      style={{
                                        display: 'inline-block',
                                        maxWidth: 560,
                                        overflow: 'hidden',
                                        textOverflow: 'ellipsis',
                                        whiteSpace: 'nowrap',
                                        verticalAlign: 'bottom',
                                      }}
                                    >
                                      {d.location ?? '—'}
                                    </span>
                                  </Button>
                                </Tooltip>
                              </li>
                            ))}
                          </ul>
                          {g.count > samples.length ? (
                            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                              {t('另有')} {g.count - samples.length} {t('处同类，点「查看明细」按此类型筛选')}
                            </Typography.Text>
                          ) : null}
                        </div>
                      ) : null}
                    </Card>
                  );
                })}
              </>
            )
          ) : (
            <>
              {total > listed ? (
                <Alert
                  type="info"
                  showIcon
                  style={{ marginBottom: 12 }}
                  message={t('明细已被读取上限截断')}
                  description={
                    `${t('本工程共')} ${total} ${t('条记录，当前列出了')} ${listed} ${t('条（明细有读取上限）：按类型的计数取自全量汇总，仍然准确。')}`
                  }
                />
              ) : null}
              <Space wrap style={{ marginBottom: 12 }}>
                <Segmented
                  size="small"
                  value={severity}
                  onChange={(v) => setSeverity(v as Severity | 'all')}
                  options={[
                    { label: t('全部'), value: 'all' },
                    ...SEVERITY_ORDER.map((sev) => ({
                      label: `${t(SEVERITY_LABEL[sev])} ${s?.[sev] ?? 0}`,
                      value: sev,
                    })),
                  ]}
                />
                {/* 类型用下拉而不是 Segmented：诊断类型可随 FKB 增加，
                    Segmented 会在窄屏（本项目常见的侧栏 + 画布布局）横向溢出，
                    而 Select 自己会截断。 */}
                <Select
                  size="small"
                  style={{ minWidth: 280 }}
                  value={code}
                  onChange={setCode}
                  showSearch
                  optionFilterProp="label"
                  options={[
                    { label: t('全部类型'), value: 'all' },
                    ...groups.map((g) => ({
                      label: `${codeTitle(g.code)}（${g.count}）`,
                      value: g.code,
                    })),
                  ]}
                />
              </Space>
              <Table<Diagnostic>
                rowKey={(d, i) => `${d.code}-${d.location ?? ''}-${i ?? 0}`}
                loading={list.loading && !list.data}
                dataSource={rows}
                pagination={{ pageSize: 20 }}
                locale={{ emptyText: t('暂无建图报告条目') }}
                columns={[
                  {
                    title: t('严重度'),
                    dataIndex: 'severity',
                    width: 90,
                    render: (sev: Severity) => (
                      <Tag color={SEVERITY_COLOR[sev]}>{t(SEVERITY_LABEL[sev])}</Tag>
                    ),
                  },
                  {
                    title: t('类型'),
                    dataIndex: 'code',
                    width: 260,
                    render: (c: string) => (
                      <Space direction="vertical" size={0}>
                        <span>{codeTitle(c)}</span>
                        <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                          <code>{c}</code>
                        </Typography.Text>
                      </Space>
                    ),
                  },
                  { title: t('阶段'), dataIndex: 'phase', width: 120 },
                  {
                    title: t('位置'),
                    dataIndex: 'location',
                    width: 300,
                    ellipsis: true,
                    render: (loc: string | null, d) =>
                      loc ? (
                        <Tooltip title={d.message}>
                          <Button
                            type="link"
                            size="small"
                            icon={<CopyOutlined />}
                            onClick={() => copyLocation(loc)}
                            style={{ paddingInline: 4 }}
                          >
                            {loc}
                          </Button>
                        </Tooltip>
                      ) : (
                        <Typography.Text type="secondary">—</Typography.Text>
                      ),
                  },
                  { title: t('说明'), dataIndex: 'message' },
                ]}
              />
            </>
          )}
        </Card>
      ) : null}
    </>
  );
}
