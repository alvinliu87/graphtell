import { useState } from 'react';
import { Alert, Button, Card, Input, Select, Space, Switch, Tag, Tooltip, Typography } from 'antd';
import { useNavigate } from 'react-router-dom';
import {
  IDE_LABEL,
  IdeTarget,
  ROOT_TEMPLATE_KEY,
  WSL_CONFIGURED_KEY,
  WSL_DISTRO_KEY,
  WSL_MODE_KEY,
  absolutePath,
  applyWslAuto,
  effectiveTemplate,
  getRootTemplate,
  getWslConfigured,
  getWslDistro,
  getWslMode,
  preferredIde,
  resolveProjectRoot,
  setPreferredIde,
  setRootTemplate,
  setWslDistro,
  setWslMode,
} from '@/shared/lib/ide';
import { getCachedBackendEnv } from '@/shared/lib/backendEnv';
import { useLocale } from '@/shared/lib/i18n';

const TEMPLATE_EXAMPLES: { label: string; value: string }[] = [
  { label: 'WSL（distro=Ubuntu）', value: '\\\\wsl$\\Ubuntu{root}' },
  { label: 'Docker 挂载 /host', value: '/host{root}' },
  { label: '远程 dev server', value: '/remote{root}' },
];

/** 设置页：全局「根模板」（后端根 → 本地根的通用变换）+ WSL 快捷预设 + 默认 IDE。 */
export function SettingsPage() {
  const navigate = useNavigate();
  const { t } = useLocale();
  const [wsl, setWsl] = useState<boolean>(() => getWslMode());
  const [distro, setDistro] = useState<string>(() => getWslDistro());
  const [template, setTemplate] = useState<string>(() => getRootTemplate());
  const [ide, setIde] = useState<IdeTarget>(() => preferredIde());

  // 预览：用一段示例后端根 + 示例相对文件，演示解析后的本地路径。
  // 实际生效的模板由 effectiveTemplate 决定（WSL 开启时自动为 {root}）。
  const sampleBackend = '/home/alvin/graphtell';
  const sampleFile = 'ui/src/pages/graph/GraphPage.tsx';
  const sampleLine = 10;
  const resolvedRoot = resolveProjectRoot(sampleBackend, '', effectiveTemplate());
  const resolvedFile = absolutePath(sampleFile, resolvedRoot);
  const wslPreview =
    wsl && distro
      ? `vscode://vscode-remote/wsl+${distro}${absolutePath(sampleFile, sampleBackend)}:${sampleLine}`
      : null;

  // 后端探测到的环境（应用启动时由 /api/health 拉取并缓存）。
  const env = getCachedBackendEnv();
  const autoWsl = env ? env.isWsl && env.clientPlatform === 'windows' : null;
  const wslConfigured = getWslConfigured();

  // 恢复为后端自动探测结果（清除手动配置标记后重新套用）。
  const restoreAuto = () => {
    try {
      localStorage.removeItem(WSL_CONFIGURED_KEY);
    } catch {
      /* ignore */
    }
    if (env) applyWslAuto(autoWsl ?? false, env.wslDistro);
    setWsl(getWslMode());
    setDistro(getWslDistro());
  };

  return (
    <Card variant="borderless" style={{ borderRadius: 14, maxWidth: 760 }}>
      <Typography.Title level={4} style={{ marginTop: 0 }}>
        {t('设置')}
      </Typography.Title>

      {/* 暂时注释：WSL 快捷配置 / 全局根模板 / 默认 IDE 等设置项停用（IDE 打开入口已移除，
          无需再处理「后端根 → Windows 本地根」的映射）。以后再考虑加回：去掉下方注释块的起止标记即可恢复。 */}
      <Alert
        type="info"
        showIcon
        message={t('设置项暂未启用')}
        description={t('本地根模板 / WSL 模式 / 默认 IDE 等设置仅服务于「跳转 IDE」；该入口已移除，相关设置暂时停用。')}
      />

      {/*
        ===== 以下为暂时注释的设置项（WSL 快捷配置 / 全局根模板 / 预览 / 默认 IDE / 按工程特例）=====
        WSL 快捷预设：开关 + 发行版名，零填写即可让 WSL 分析的工程一键跳转。
      <Card size="small" style={{ background: '#f8fafc' }} title={t('WSL 快捷配置')}>
        <Space direction="vertical" size={10} style={{ width: '100%' }}>
          <Space size={10} align="center">
            <Switch checked={wsl} onChange={setWsl} />
            <Typography.Text strong>{t('启用 WSL 模式')}</Typography.Text>
          </Space>
          <Space size={8} align="center" wrap>
            <Typography.Text type="secondary" style={{ fontSize: 13 }}>
              {t('发行版（distro）')}
            </Typography.Text>
            <Input
              size="small"
              style={{ width: 160 }}
              value={distro}
              disabled={!wsl}
              onChange={(e) => setDistro(e.target.value)}
              placeholder="Ubuntu"
            />
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {t('如 ') + '\\\\wsl$\\<distro>'}
            </Typography.Text>
          </Space>
          <Typography.Paragraph type="secondary" style={{ fontSize: 12, margin: 0 }}>
            {t('开启后无需手填模板：各工程的后端 Linux 路径会自动按 WSL 处理——VS Code / Cursor 走')}
            <Typography.Text code>vscode://vscode-remote/wsl+&lt;distro&gt;</Typography.Text>
            {t('远程 scheme，JetBrains 与"复制路径"补')}
            <Typography.Text code>{'\\\\wsl$\\<distro>'}</Typography.Text>
            {t('前缀。')}
          </Typography.Paragraph>
          {wslPreview ? (
            <Typography.Text style={{ fontSize: 12 }}>
              {t('示例跳转 URL：')}<Typography.Text code>{wslPreview}</Typography.Text>
            </Typography.Text>
          ) : null}
          {env ? (
            <Space size={8} align="center" wrap>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('后端探测：')}
                {env.isWsl ? (
                  <Typography.Text code style={{ fontSize: 12 }}>
                    {t('WSL（') + env.wslDistro + t('）')}
                  </Typography.Text>
                ) : (
                  <Typography.Text code style={{ fontSize: 12 }}>
                    {t('非 WSL')}
                  </Typography.Text>
                )}
                {t('，客户端：')}{env.clientPlatform}
              </Typography.Text>
              {wslConfigured ? (
                <Button size="small" onClick={restoreAuto}>
                  {t('恢复自动探测')}
                </Button>
              ) : (
                <Tag color="green" style={{ margin: 0 }}>
                  {t('已自动套用')}
                </Tag>
              )}
            </Space>
          ) : null}
        </Space>
      </Card>

      <Typography.Paragraph type="secondary" style={{ fontSize: 13, marginTop: 16 }}>
        {t('通用工具不内嵌任何场景（WSL / Docker / 远程）的假设。用「根模板」描述')}
        <Typography.Text code>{t('后端 root_path → 本地根')}</Typography.Text>
        {t('的变换，')}<Typography.Text code>{'{root}'}</Typography.Text>
        {t('会被替换为后端工程根；留空则不启用，回退到后端路径。WSL 模式开启时此模板被忽略。')}
      </Typography.Paragraph>

      <Space direction="vertical" size={6} style={{ width: '100%' }}>
        <Typography.Text strong>{t('全局根模板（非 WSL 场景 / 高级）')}</Typography.Text>
        <Input
          size="large"
          value={template}
          disabled={wsl}
          placeholder={t('例如 \\\\wsl$\\Ubuntu{root}（留空则不启用）')}
          onChange={(e) => setTemplate(e.target.value)}
        />
        <Space wrap size={6}>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {t('示例一键填入：')}
          </Typography.Text>
          {TEMPLATE_EXAMPLES.map((ex) => (
            <Tag
              key={ex.value}
              color="blue"
              style={{ cursor: wsl ? 'not-allowed' : 'pointer', opacity: wsl ? 0.5 : 1 }}
              onClick={() => !wsl && setTemplate(ex.value)}
            >
              {t(ex.label)}
            </Tag>
          ))}
        </Space>
      </Space>

      <Card
        size="small"
        style={{ marginTop: 16, background: '#f8fafc' }}
        title={t('预览（示例后端根 /home/alvin/graphtell）')}
      >
        <Space direction="vertical" size={4} style={{ width: '100%' }}>
          <Typography.Text style={{ fontSize: 13 }}>
            {t('解析后的本地根：')}
            <Typography.Text code>{resolvedRoot || t('（无，将使用后端路径）')}</Typography.Text>
          </Typography.Text>
          <Typography.Text style={{ fontSize: 13 }}>
            {t('示例文件 ') + sampleFile + ' →'}
            <Typography.Text code>{resolvedFile}</Typography.Text>
          </Typography.Text>
        </Space>
      </Card>

      <Typography.Title level={5} style={{ marginTop: 24 }}>
        {t('默认 IDE')}
      </Typography.Title>
      <Select<IdeTarget>
        style={{ width: 240 }}
        value={ide}
        options={(Object.keys(IDE_LABEL) as IdeTarget[]).map((t) => ({
          value: t,
          label: IDE_LABEL[t],
        }))}
        onChange={(t) => {
          setIde(t);
          setPreferredIde(t);
        }}
      />
      <Tooltip title={t('当前默认：') + IDE_LABEL[ide]}>
        <Typography.Text type="secondary" style={{ fontSize: 12, marginLeft: 10 }}>
          {t('点击任意位置跳转时会优先用它')}
        </Typography.Text>
      </Tooltip>

      <Space style={{ marginTop: 20 }}>
        <Button
          type="primary"
          onClick={() => {
            setWslMode(wsl);
            setWslDistro(distro);
            setRootTemplate(wsl ? '' : template);
            navigate('/');
          }}
        >
          {t('保存设置')}
        </Button>
        {!wsl && template.trim() ? (
          <Button onClick={() => setTemplate('')}>{t('清除模板')}</Button>
        ) : null}
      </Space>

      <Alert
        type="info"
        showIcon
        style={{ marginTop: 20 }}
        message={t('按工程特例')}
        description={t('若某个工程的本地根无法用模板表达（盘符 / 目录完全不同），可在其图视图页的「本地工程根（覆盖）」单独填写，覆盖全局设置。')}
      />
      <Typography.Paragraph type="secondary" style={{ fontSize: 11, marginTop: 12 }}>
        {t('存储键：')}{WSL_MODE_KEY}、{WSL_DISTRO_KEY}、{ROOT_TEMPLATE_KEY}{t('（浏览器 localStorage，仅本机生效）')}
      </Typography.Paragraph>
        ===== 暂时注释结束（以后再考虑加回）=====
      */}
    </Card>
  );
}
