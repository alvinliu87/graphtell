/** 建图流水线实体。 */

export interface PhaseReport {
  phase: string;
  nodes_created: number;
  edges_created: number;
  annotations_created: number;
  aliases_created: number;
  duration_ms: number;
  diagnostics: number;
}

export interface RunStatus {
  project_id: number;
  current_phase: string | null;
  phases: PhaseReport[];
  status: string;
}

export interface RunAccepted {
  project_id: number;
  accepted: boolean;
}

/** 阶段顺序（与后端 P0→P7 一致）。 */
export const PHASE_ORDER = [
  'Ingest',
  'CfAst',
  'Prepare',
  'AnnotatePre',
  'Synthesize',
  'AnnotatePost',
  'Resolve',
];

export const PHASE_LABEL: Record<string, string> = {
  Ingest: '摄取',
  CfAst: '语法建图',
  Prepare: '知识装载',
  AnnotatePre: '源码标注',
  Synthesize: '语义合成',
  AnnotatePost: '汇聚标注',
  Resolve: '动态解析',
};

export const PHASE_HINT: Record<string, string> = {
  Ingest: '识别子工程与待分析文件，排除依赖目录与静态资源',
  CfAst: '从语言语法创建 Class / Method / Property / CallSite 节点与继承边',
  Prepare: '装载框架知识，解析 AppRoot、容器绑定、事件表、数据库 schema 等权威源',
  AnnotatePre: '按框架知识规则给调用点打安全标记与框架语义标签',
  Synthesize: '合成 Table / HttpContract / ConfigKey / Event 等语义节点并幂等合并',
  AnnotatePost: '在汇聚结果上打隐私字段、关键度、配置可变性等标签并注册别名',
  Resolve: '漏斗式解析容器/事件/门面/路由，建立动态边',
};

export interface HealthDto {
  status: string;
  version: string;
  languages: string[];
  frameworks: number;
}
