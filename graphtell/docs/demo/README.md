# GraphTell 静态 Demo

这是用**真实产品前端 + 录制 API 回放**生成的纯静态站点（无后端依赖），
可直接托管在 GitHub Pages 等任意静态空间。

## 这个 demo 里能做什么
- **浏览工程**：首页列出已录制的示例工程（CRMEB / Bagisto / 自造夹具）。
- **语义图**：进入某工程的「图」页签，看路由/表/事件等视角的语义依赖图；
  点节点可展开链路（对象视图）。
- **规则检验**：`合规` 页签看规则命中与诊断。
- **提示词增强**：`召回 / 提示词` 页签，对**预置的中文问句**做代码召回并合成提示词。

## 界面预览（真机截图，来自本 demo 本地运行）
![工程总览](../screenshots/home.png)
![语义图](../screenshots/graph-crmeb.png)
![规则检验：违规表](../screenshots/rules-crmeb.png)
![提示词增强](../screenshots/recall.png)

## 局限性（设计如此，非 bug）
- 写操作（新建/删除工程、跑建图、文件浏览）在静态站上不可用——它们需要真后端。
- 召回/提示词的**输入框里只能选预置问句**会命中录制；任意新输入的问句会回落到某条
  预置结果（页面不报错，但答案不是你输入的那个）。
- 未经录制到的深度请求（比如手动改 URL 跳到一个没录的节点/视角）会拿到空或「全局视角」
  兜底响应，页面降级而非崩溃。

## 想换成你自己的样本 / 重新生成
```bash
# 默认样本（需先 cargo build -p gt-app）
python3 tools/gen_ui_demo.py

# 指定样本（name=绝对路径）
python3 tools/gen_ui_demo.py --sample "我的项目=/abs/path/to/code"

# 跳过前端构建（已构建过时）
python3 tools/gen_ui_demo.py --no-build
```
生成物在 `docs/demo/`，整套作为站点根目录托管即可（已用相对路径 + hash 路由，
挂在 `https://<user>.github.io/<repo>/` 子路径下也能正常工作）。
