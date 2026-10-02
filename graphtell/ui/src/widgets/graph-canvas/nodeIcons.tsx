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

/** Minimal props type accepted by icon components (antd icons satisfy this superset). */
export type IconComp = ComponentType<{
  width?: number | string;
  height?: number | string;
  style?: CSSProperties;
}>;

/**
 * Icon per node kind (from @ant-design/icons — nothing is hand-drawn).
 * The renderer gets the component via `nodeIcon(kind)`; the colour is injected by the caller from the
 * kind colour (nodeColor). Unknown kinds fall back to QuestionOutlined so an icon is never missing.
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
  // Middleware: the gatekeeper attached to a route — a shield icon, semantically distinct from
  // "cache / config / contract".
  Middleware: SafetyOutlined,
  // Event / queue consumers are relabelled to this role on the canvas (see the view-layer relabelling
  // in view_service); they had no icon before and always fell back to a question mark.
  EventHandler: NotificationOutlined,
  Unknown: QuestionOutlined,
};

export function nodeIcon(kind: string): IconComp {
  return NODE_ICONS[kind] ?? QuestionOutlined;
}

/** For the legend: export only the kinds actually used (deduped against the given list, preserving order). */
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
