import type { CSSProperties, ComponentType } from 'react';
import {
  AimOutlined,
  ApiOutlined,
  AppstoreOutlined,
  BranchesOutlined,
  BuildOutlined,
  CalendarOutlined,
  CloudOutlined,
  CodeOutlined,
  DatabaseOutlined,
  DeploymentUnitOutlined,
  FileOutlined,
  FolderOutlined,
  FunctionOutlined,
  LockOutlined,
  MessageOutlined,
  NotificationOutlined,
  PropertySafetyOutlined,
  QuestionOutlined,
  SafetyOutlined,
  SettingOutlined,
  TagOutlined,
  ThunderboltOutlined,
  TranslationOutlined,
  UnorderedListOutlined,
} from '@ant-design/icons';

/** 图标组件接受的极简 props 类型（antd 图标兼容此超集）。 */
export type IconComp = ComponentType<{
  width?: number | string;
  height?: number | string;
  style?: CSSProperties;
}>;

/**
 * 每种节点 kind 对应的图标（@ant-design/icons，无需自绘）。
 * 渲染端用 `nodeIcon(kind)` 取组件；颜色由调用方按 kind 色（nodeColor）注入。
 * 找不到时回退到 QuestionOutlined，避免缺图。
 */
export const NODE_ICONS: Record<string, IconComp> = {
  File: FileOutlined,
  Directory: FolderOutlined,
  Namespace: AppstoreOutlined,
  Class: BuildOutlined,
  Interface: DeploymentUnitOutlined,
  Trait: BranchesOutlined,
  Enum: UnorderedListOutlined,
  EnumCase: TagOutlined,
  Method: FunctionOutlined,
  Function: CodeOutlined,
  Property: PropertySafetyOutlined,
  Const: LockOutlined,
  CallSite: AimOutlined,
  Table: DatabaseOutlined,
  HttpContract: ApiOutlined,
  ConfigKey: SettingOutlined,
  I18nKey: TranslationOutlined,
  Event: NotificationOutlined,
  Schedule: CalendarOutlined,
  Queue: CloudOutlined,
  Cache: ThunderboltOutlined,
  Topic: MessageOutlined,
  // 中间件：挂在路由上的守门人 —— 盾牌图标，与"缓存 ⚡ / 配置 ⚙ / 契约 🔗"的语义区分开。
  Middleware: SafetyOutlined,
  // 事件 / 队列的消费者在画布上重标为这个角色（见 view_service 的视图层重标），
  // 此前没有图标、一直回退成问号。
  EventHandler: NotificationOutlined,
  Unknown: QuestionOutlined,
};

export function nodeIcon(kind: string): IconComp {
  return NODE_ICONS[kind] ?? QuestionOutlined;
}

/** 图例用：仅导出已被用到的 kind（按传入列表去重，保持出现顺序）。 */
export function usedKinds(kinds: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const k of kinds) {
    if (!seen.has(k)) {
      seen.add(k);
      out.push(k);
    }
  }
  return out;
}
