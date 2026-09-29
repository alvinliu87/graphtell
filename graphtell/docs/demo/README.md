# GraphTell 静态 Demo

真实产品前端 + 录制 API 回放的纯静态站点（无后端依赖），可托管在 GitHub Pages。

## 能做什么
- 浏览工程（CRMEB / Bagisto / 自造夹具）
- 语义图：进入工程「图」页签，看路由/表/事件等视角的语义依赖图，点节点展开链路
- 规则检验：合规页签看规则命中与诊断
- 提示词增强：召回/提示词页签，对预置中文问句做代码召回并合成提示词

## 局限（设计如此）
- 写操作（新建/删除工程、跑建图、文件浏览）需真后端，静态站不可用
- 召回/提示词只能选预置问句命中录制；任意新输入会回落到某条预置结果
- 未录制的深度请求会拿到空或「全局视角」兜底，页面降级而非崩溃

## 重新生成
    python3 tools/gen_ui_demo.py                 # 默认样本（先 cargo build -p gt-app）
    python3 tools/gen_ui_demo.py --sample 我的项目=/abs/path
    python3 tools/gen_ui_demo.py --no-build
