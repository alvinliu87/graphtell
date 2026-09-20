//! P7 Resolve：建立动态边。
//!
//! 核心是**解析漏斗 + 不动点迭代**：
//!
//! | 层级 | 来源                                  | 置信度 |
//! | ---- | ----------------------------------- | --- |
//! | L1   | 字面 FQN `make(StoreOrderServices::class)` | 1.0 |
//! | L2   | 容器注册表 `provider.php`                | 0.95 |
//! | L3   | 别名索引（Facade / 事件名 / 获取器）           | 0.85 |
//! | L4   | 约定（命名空间拼接、类名推导）                    | 0.8  |
//! | L5   | 常量传播                                | 0.6  |
//! | L6   | 与有限全集求交（schema 的 203 张表）            | 0.7  |
//! | L7   | 完全未知                                | 0.3  |
//!
//! 迭代判据：**候选集变大才继续下一轮**，直到收敛；收敛后再统一建边。

use std::collections::{HashMap, HashSet};

use gt_domain::model::{
    EdgeKind, FactValue, HandlerSpec, Language, NewEdge, NodeId, NodeKind, Phase, ResolveStrategy,
    ResolveTier, Resolution, Severity, SubProjectId,
};
use gt_domain::port::KnowledgeProvider;
use serde_json::Value;
use tracing::info;

use crate::context::PipelineContext;
use crate::workspace::CallRecord;

/// 一次动态解析请求（去重后参与不动点迭代）。
#[derive(Debug, Clone)]
struct Locator {
    owner: NodeId,
    /// 所属方法的 FQN（供按"变量类型"解析时查参数 / 属性类型）。
    owner_fqn: String,
    strategy: ResolveStrategy,
    raw: String,
    /// 变量类型策略用：接收者（`$services` / `$this->services`）。
    receiver: Option<String>,
    /// 变量类型策略用：方法名。
    method: Option<String>,
    /// `EventListen` 策略用：监听器类（arg1，如 `OrderListener::class`）。
    consumer: Option<String>,
    file: String,
    line: u32,
    /// 所属子工程：用于取该子工程的路由 handler 解析规则（FKB 声明）。
    sub: Option<SubProjectId>,
}

/// 执行 Resolve。
pub fn run(ctx: &mut PipelineContext, kb: &dyn KnowledgeProvider) {
    let phase = Phase(Phase::RESOLVE.to_string());

    // ① 先解析 P5 遗留的待定链接（路由 handler 等）
    resolve_pending_links(ctx, &phase);

    // ①.5 通用调用解析：建立 `Calls` 边，把「路由 → handler → 服务方法 → 语义节点」
    //      的调用链真正连通（否则语义边孤立在被调方法上，视图只能靠结构边绕路）。
    resolve_calls(ctx, &phase);

    // ①.6 门面链式读 / 写动词：`Db::name('x')->insert()/find()`。末端动词落在未标注
    //      类型的 Query 上推不出表，但链内 `name('x')` 已透传表名（parser 取得），
    //      直接落成 `WritesDb` / `ReadsDb`，避免这类入口退回含糊的「映射到」。
    classify_facade_db_calls(ctx);

    // ② 收集 FKB 声明的动态调用
    let resolvers: Vec<(String, ResolveStrategy)> = ctx
        .frameworks
        .values()
        .flatten()
        .filter_map(|id| kb.by_id(id))
        .flat_map(|fk| fk.resolvers.iter())
        .map(|r| (r.call.clone().unwrap_or_default(), r.strategy))
        .filter(|(call, _)| !call.is_empty())
        .collect();

    // 按 (strategy, raw) 去重：同一个字符串只需解析一次
    let mut uniques: Vec<Locator> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    for call in ctx.ws.calls.iter() {
        for (pattern, strategy) in &resolvers {
            if !crate::engine::aliased_callee_matches(&ctx.ws, pattern, call) {
                continue;
            }
            // 变量类型：按「所属方法 + 接收者 + 方法」建定位点，不依赖参数。
            if *strategy == ResolveStrategy::VariableType {
                let (Some(recv), Some(method)) =
                    (call.receiver.as_deref(), call.method.as_deref())
                else {
                    continue;
                };
                // PHP：接收者须为 `$var` / `$this->prop` / `(new X)`（链式 builder
                // `(new static)->select()` 的接收者被 parser 记成整个 `new` 表达式，
                // 需当作变量类型解析，才能把末端读 / 写动词落成 `ReadsDb` / `WritesDb`）；
                // 静态 `Foo::m()` 交给 Facade。Java：字段是裸标识符，按字段声明类型解析。
                let is_java = call.language.as_str() == Language::JAVA;
                if (!recv.starts_with('$') && !recv.starts_with("(new ") && !is_java)
                    || method.is_empty()
                {
                    continue;
                }
                let raw = format!("{}::{recv}::{method}", call.owner_fqn);
                if keys.insert(format!("{strategy:?}:{raw}")) {
                    uniques.push(Locator {
                        owner: call.owner,
                        owner_fqn: call.owner_fqn.clone(),
                        strategy: *strategy,
                        raw,
                        receiver: Some(recv.to_string()),
                        method: Some(method.to_string()),
                        consumer: None,
                        file: call.file.clone(),
                        line: call.span.start_line,
                        sub: call.sub,
                    });
                }
                continue;
            }
            let raw = call.args.first().map(fact_to_string).unwrap_or_default();
            if raw.is_empty() || raw.len() > 256 {
                continue;
            }
            // `EventListen` 需要 arg1（监听器类）作为边的另一端。
            let consumer = if *strategy == ResolveStrategy::EventListen {
                call.args.get(1).map(fact_to_string)
            } else {
                None
            };
            let key = format!("{strategy:?}:{raw}");
            if keys.insert(key) {
                uniques.push(Locator {
                    owner: call.owner,
                    owner_fqn: call.owner_fqn.clone(),
                    strategy: *strategy,
                    raw,
                    receiver: call.receiver.clone(),
                    method: call.method.clone(),
                    consumer,
                    file: call.file.clone(),
                    line: call.span.start_line,
                    sub: call.sub,
                });
            }
        }
    }

    // ③ 不动点迭代：直到没有 locator 的候选集再变大
    let mut resolutions: HashMap<String, Resolution> = HashMap::new();
    let mut rounds = 0usize;
    const MAX_ROUNDS: usize = 16;
    loop {
        rounds += 1;
        let mut grew = false;
        for loc in &uniques {
            let key = format!("{:?}:{}", loc.strategy, loc.raw);
            let before = resolutions.get(&key).map(|r| r.candidates.len()).unwrap_or(0);
            let res = resolve_once(ctx, loc);
            if res.candidates.len() > before {
                resolutions.insert(key, res);
                grew = true;
            } else if !resolutions.contains_key(&key) {
                resolutions.insert(key, res);
            }
        }
        if !grew || rounds >= MAX_ROUNDS {
            break;
        }
    }

    // ④ 收敛后统一建边与记账
    for loc in &uniques {
        let key = format!("{:?}:{}", loc.strategy, loc.raw);
        if let Some(res) = resolutions.get(&key) {
            apply_resolution(ctx, loc, res, &phase);
        }
    }

    info!("P7 解析完成：{} 个解析点，{} 轮迭代", uniques.len(), rounds);
}

fn resolve_once(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match loc.strategy {
        ResolveStrategy::Container => resolve_container(ctx, loc),
        ResolveStrategy::Event => resolve_event(ctx, loc),
        ResolveStrategy::EventListen => resolve_event_listen(ctx, loc),
        ResolveStrategy::Facade => resolve_facade(ctx, loc),
        ResolveStrategy::Accessor => resolve_accessor(ctx, loc),
        ResolveStrategy::Handler => resolve_handler(ctx, loc),
        ResolveStrategy::VariableType => resolve_variable_type(ctx, loc),
    }
}

/// 变量类型解析：`$svc->method()` / `$this->prop->method()` → `Type::method`。
///
/// 类型来源（由 P2 记录）：
/// * `$var` —— 所在方法的**参数类型提示**（ThinkPHP 控制器/服务常见 DI 写法）；
/// * `$this->prop` —— 构造器注入（`__construct(T $x){ $this->p = $x; }`）。
///
/// 找不到类型就返回 unknown —— **宁可缺边，不可错边**。
fn resolve_variable_type(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let Some(recv) = loc.receiver.as_deref() else {
        return Resolution::unknown("无接收者".to_string());
    };
    let method = loc.method.as_deref().unwrap_or("");
    if method.is_empty() {
        return Resolution::unknown("无方法名".to_string());
    }
    let type_fqn = receiver_type_fqn(ctx, &loc.owner_fqn, recv);
    let Some(type_fqn) = type_fqn else {
        return Resolution::unknown(format!("接收者 {recv} 的类型未知"));
    };

    // 目标方法：先本类，再沿继承链回溯（方法常继承自基类）。
    // 成员分隔符语言相关：PHP `Class::method`、Java `Class.method` —— 两者都试。
    let mut stack = vec![type_fqn.clone()];
    let mut visited: HashSet<String> = HashSet::new();
    let mut steps = 0;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > 50 || !visited.insert(t.clone()) {
            continue;
        }
        let candidates = [
            format!("{t}::{method}"),
            format!("{t}.{method}"),
        ];
        if let Some(id) = candidates.iter().find_map(|f| ctx.ws.find_by_name(f)) {
            // 调用落在数据模型的读 / 写动词上时，把「模型映射到表」升级成真正的动作边：
            // `$goodsModel->save()` 不是含糊的「映射到 goods」，而是 `WritesDb → goods`。
            // 边与传播种子都在这里落（P8 才能把动作沿调用链带到入口）。
            classify_db_action(ctx, loc, &type_fqn, method);
            return Resolution::resolved(
                ResolveTier::Convention,
                id,
                format!("按变量类型解析 {} -> {method}", t),
            );
        }
        for parent in ctx.ws.parents_of(&t) {
            stack.push(parent);
        }
    }
    // 兜底：**魔法方法转发** —— 接收者类型（或其祖先）用 `@method` 声明了这个方法，
    // 实际由 `__call` 转发给 FKB 声明的属性（CRMEB 是 `$this->dao`）。
    // 不解析它，`$this->services->getList()` 只能退回"类级命中"，服务这一跳就断了
    // （CRMEB 的 20 多个 `get*` / `count*` / `delete*` 全是这种情况）。
    let magic = resolve_magic_delegation(ctx, loc, &type_fqn, method);
    // 即便调用本身没解析出目标方法，只要它命中了 FKB 声明的读 / 写动词，也应落成动作边。
    // 接收者类型优先用「解析出的接收者类型」（`(new X)->value()` 里 X 是模型、`$this`
    // 是模型），回退到「所在类」—— 典型如模型方法里的 `$this->where()->find()`：
    // `where()` 返回未标注类型的 Query，链式末端动词推不出接收者类型，但 owner 类就是
    // 被映射的模型。（之前只用 owner 类会漏掉 `(new ModelX)->value()` 这类接收者是模型、
    // 但方法写在 service 里的读 / 写，导致路由退回含糊的「映射到」。）
    if magic.is_none() && is_db_verb(ctx, loc, method).is_some() {
        classify_db_action(ctx, loc, &type_fqn, method);
    }
    magic.unwrap_or_else(|| Resolution::unknown(format!("{type_fqn}::{method} 未找到")))
}

/// 取 `app\model\User::login` 这样的 FQN 里的「类部分」。
fn owner_class_fqn(owner_fqn: &str) -> String {
    match owner_fqn.rfind("::") {
        Some(i) => owner_fqn[..i].to_string(),
        None => owner_fqn.to_string(),
    }
}

/// 方法名是否命中 FKB 声明的读 / 写动词；命中则返回应落的边种类。
fn is_db_verb(
    ctx: &PipelineContext,
    loc: &Locator,
    method: &str,
) -> Option<(EdgeKind, &'static str)> {
    let spec = loc
        .sub
        .and_then(|s| ctx.db_verbs.get(&s.get()).cloned())
        .or_else(|| ctx.db_verbs_default.clone())
        .filter(|s| !s.write.is_empty() || !s.read.is_empty())?;
    let m = method.to_ascii_lowercase();
    if spec.write.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
        Some((EdgeKind(EdgeKind::WRITES_DB.to_string()), "写"))
    } else if spec.read.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
        Some((EdgeKind(EdgeKind::READS_DB.to_string()), "读"))
    } else {
        None
    }
}

/// 魔法方法转发：`Services::getList` → `Services` 的 `dao` 属性类型上的 `getList`。
///
/// 前置条件（都由 FKB / 解析器给出，内核不认识任何框架）：
/// * FKB 声明了转发属性名（`magic_delegation.property`）；
/// * 该类或其祖先用 `@method` 声明了这个方法名；
/// * 转发属性的类型可静态确定（构造器注入 / 类型化属性 / 赋值推断）。
///
/// 顺带：若被转发的方法本身是 FKB 声明的读 / 写动词（CRMEB 的 `$this->save()` 经
/// `__call` 转发给 `dao->save`；`dao` 映射到表），则把转发落点（`dep` 类型）当成数据模型
/// 分类，直接落成 `WritesDb` / `ReadsDb` —— 否则这类写操作只能退回含糊的「映射到」
/// （CRMEB 的 156 条「映射」几乎都来自此）。
fn resolve_magic_delegation(
    ctx: &mut PipelineContext,
    loc: &Locator,
    type_fqn: &str,
    method: &str,
) -> Option<Resolution> {
    let spec = loc
        .sub
        .and_then(|s| ctx.magic_delegation.get(&s.get()).cloned())
        .or_else(|| ctx.magic_delegation_default.clone())?;
    if spec.property.is_empty() || !ctx.ws.declares_magic_method(type_fqn, method) {
        return None;
    }
    // 转发属性的类型按**具体接收者类型**取（子类构造器注入的是它自己的 Dao）。
    let mut cur = Some(type_fqn.to_string());
    let mut visited: HashSet<String> = HashSet::new();
    let mut steps = 0;
    while let Some(c) = cur {
        steps += 1;
        if steps > 50 || !visited.insert(c.clone()) {
            break;
        }
        if let Some(dep) = ctx.ws.prop_type(&c, &spec.property) {
            let dep = dep.to_string();
            // 被转发的方法若是读 / 写动词，把 `dep`（它的真实类型，映射到表）当数据模型分类。
            if is_db_verb(ctx, loc, method).is_some() {
                classify_db_action(ctx, loc, &dep, method);
            }
            // 目标方法同样沿继承链回溯（Dao 的方法多定义在 `BaseDao`）。
            let mut stack = vec![dep.clone()];
            let mut seen: HashSet<String> = HashSet::new();
            let mut n = 0;
            while let Some(t) = stack.pop() {
                n += 1;
                if n > 50 || !seen.insert(t.clone()) {
                    continue;
                }
                for cand in [format!("{t}::{method}"), format!("{t}.{method}")] {
                    if let Some(id) = ctx.ws.find_by_name(&cand) {
                        return Some(Resolution {
                            candidates: vec![id],
                            tier: ResolveTier::Convention,
                            confidence: spec.confidence,
                            evidence: format!(
                                "魔法方法转发 {type_fqn}::{method} → {dep}::{method}（经 {}）",
                                spec.property
                            ),
                        });
                    }
                }
                for p in ctx.ws.parents_of(&t) {
                    stack.push(p);
                }
            }
        }
        cur = ctx.ws.parents_of(&c).into_iter().next();
    }
    None
}

/// 数据模型的读 / 写动作分类（`$model->save()` → `WritesDb → goods`）。
///
/// 三个输入都是「知识」而非硬编码：
/// * 动词清单来自 FKB（`db_verbs`，ThinkPHP / Laravel 的 Model & Query API 各自声明）；
/// * 「这个类型是数据模型」由 **`MapsTo` 边**证明（P5 的模型约定规则产出的静态身份）；
/// * 两个候选类型来源都被尝试：变量类型解析得到的 `type_fqn`（如 `$this->where()` 链
///   末端推不出时退化为调用方所在的 `owner` 类 —— 模型方法里的 `$this->find()` 就是这样）。
///
/// 直接落一条 `方法 → 表` 的动作边，并登记传播种子 —— P8 会沿 `Calls` 链把它带到
/// 控制器 / 入口，路由视角于是显示「写库 / 读库」而不是含糊的「映射到」。
fn classify_db_action(
    ctx: &mut PipelineContext,
    loc: &Locator,
    type_fqn: &str,
    method: &str,
) {
    let Some((kind, _)) = is_db_verb(ctx, loc, method) else {
        return;
    };
    // 候选「数据模型类型」：
    // 1. 变量类型解析出的接收者类型（如 `$this->save` 落在模型类上）；
    // 2. 调用方所在类（模型方法里的 `$this->find` 接收者推不出时退化到这里）；
    // 3. 经 FKB 声明的魔法转发属性（service → `dao`）可达的类型 —— service 类本身
    //    不映射到表，但它的 `dao` 属性类型（继承链上）映射到了表。CRMEB 的
    //    `$this->save()` 正是这种情况：接收者是 service，真正的表在 `dao` 上。
    // 三者都试一遍（去重交给 `add_edge`）。
    let mut candidates: Vec<String> = vec![type_fqn.to_string(), owner_class_fqn(&loc.owner_fqn)];
    if let Some(spec) = loc
        .sub
        .and_then(|s| ctx.magic_delegation.get(&s.get()).cloned())
        .or_else(|| ctx.magic_delegation_default.clone())
    {
        if !spec.property.is_empty() {
            for t in [type_fqn.to_string(), owner_class_fqn(&loc.owner_fqn)] {
                if let Some(dep) = ctx.ws.prop_type(&t, &spec.property) {
                    candidates.push(dep.to_string());
                }
            }
        }
    }
    let mut tried = HashSet::new();
    for t in candidates {
        if !tried.insert(t.clone()) {
            continue;
        }
        for table in ctx.ws.mapped_tables(&t, EdgeKind::MAPS_TO) {
            emit_db_edge(
                ctx,
                loc.owner,
                loc.sub,
                &loc.owner_fqn,
                table,
                &kind,
                method,
                0.85,
                &loc_line(loc),
            );
        }
        // 回退：该类型自身没映射表，但它可能是「基类」，读 / 写动词写在基类里、
        // 实例却是映射到表的子类（`$this->select()` 在 `app\common\model\X` 基类，
        // 表挂在 `app\api\model\X` 子类）。向上查不到就向下（子类型）补查，把动词
        // 落成真正动作边，否则这些路由只能退回含糊的「映射到」。
        //
        // 但有界：CRMEB / yoshop 的「泛型基类」（`BaseModel`）被几十个模型继承，
        // 向下 BFS 会瞬间展开到几十张表，让每个调用方都「读」几十张表 —— 这是失真，
        // 不可取。所以**只有可达子类的映射表很少（≤6，典型的「每实体一个基类」）时才
        // 落边**；子类表过多说明是共享泛型基类、无法在静态分析里判明具体实例表，
        // 直接放弃（退回「映射到」，如实标注），避免动作边爆炸。
        if ctx.ws.mapped_tables(&t, EdgeKind::MAPS_TO).is_empty() {
            let mut subtype_tables: Vec<NodeId> = Vec::new();
            let mut seen_tbl: HashSet<i64> = HashSet::new();
            for sub in ctx.ws.subtypes_bfs(&t, 6, 60) {
                for table in ctx.ws.mapped_tables(&sub, EdgeKind::MAPS_TO) {
                    if seen_tbl.insert(table.get()) {
                        subtype_tables.push(table);
                    }
                }
            }
            if subtype_tables.len() <= 6 {
                for table in subtype_tables {
                    emit_db_edge(
                        ctx,
                        loc.owner,
                        loc.sub,
                        &loc.owner_fqn,
                        table,
                        &kind,
                        method,
                        0.7,
                        &loc_line(loc),
                    );
                }
            }
        }
    }
}

/// 门面链式写 / 读：`Db::name('goods')->insert()` / `Db::name('goods')->where()->find()`。
///
/// 末端动词（`insert` / `find`）落在未标注类型的 Query 上，变量类型解析推不出表；
/// 但链内的 `name('goods')` / `table('goods')` 已把表名透传到 `CallRecord.db_table`
/// （parser 沿对象链回溯取得）。这里直接用该表名反查 P5 合成出的 Table 节点，
/// 把动词落成 `WritesDb` / `ReadsDb` —— 否则这类入口只能退回含糊的「映射到」。
fn classify_facade_db_calls(ctx: &mut PipelineContext) {
    // 先收集（只读）再落边（可变），避开「迭代 `calls` 同时改 `ctx`」的借用冲突。
    // `table_id` 是 `Copy`，直接收进 vec，落边阶段无需再查。
    let mut pending: Vec<(
        NodeId,
        Option<SubProjectId>,
        String,
        String,
        EdgeKind,
        NodeId,
        String,
    )> = Vec::new();
    for call in ctx.ws.calls.iter() {
        let (Some(table_name), Some(method)) = (&call.db_table, call.method.as_deref()) else {
            continue;
        };
        let spec = call
            .sub
            .and_then(|s| ctx.db_verbs.get(&s.get()).cloned())
            .or_else(|| ctx.db_verbs_default.clone())
            .filter(|s| !s.write.is_empty() || !s.read.is_empty());
        let Some(spec) = spec else {
            continue;
        };
        let m = method.to_ascii_lowercase();
        let kind = if spec.write.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
            EdgeKind(EdgeKind::WRITES_DB.to_string())
        } else if spec.read.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
            EdgeKind(EdgeKind::READS_DB.to_string())
        } else {
            continue;
        };
        let Some(table_id) = ctx.ws.find_table_by_name(table_name) else {
            continue;
        };
        pending.push((
            call.owner,
            call.sub,
            call.owner_fqn.clone(),
            method.to_string(),
            kind,
            table_id,
            call_line(call),
        ));
    }
    for (owner, sub, owner_fqn, method, kind, table_id, location) in pending {
        emit_db_edge(
            ctx,
            owner,
            sub,
            &owner_fqn,
            table_id,
            &kind,
            &method,
            0.9,
            &location,
        );
    }
}

/// 调用点事实的 `文件:行`（供 `evidence.location`）。
fn call_line(call: &crate::workspace::CallRecord) -> String {
    format!("{}:{}", call.file, call.span.start_line)
}

/// 解析点的 `文件:行`（同上，供 `classify_db_action` 使用）。
fn loc_line(loc: &Locator) -> String {
    format!("{}:{}", loc.file, loc.line)
}

/// 落成一条 `方法 → 表` 的读 / 写动作边，并登记 P8 传播种子（去重交给 `add_edge`）。
///
/// `location`（`文件:行`）必须写进 `evidence.location`：视图靠它给出"本链路访问该资源
/// 的位置"（`view_service::inline_chain_locations` 补 `to_call_site`、`edge_evidence`
/// 取证据位置，两处都读它）。此前这里只存一句文字 scap evid，形状对不上，
/// 于是门面链式的 `Db::name('x')->update()` 边永远是一条无法跳转的死边。
fn emit_db_edge(
    ctx: &mut PipelineContext,
    owner: NodeId,
    sub: Option<SubProjectId>,
    owner_fqn: &str,
    table_id: NodeId,
    kind: &EdgeKind,
    method: &str,
    confidence: f32,
    location: &str,
) {
    let added = ctx.ws.add_edge(NewEdge {
        project_id: ctx.project.id,
        kind: kind.clone(),
        from_id: owner,
        to_id: table_id,
        phase: Phase(Phase::RESOLVE.to_string()),
        confidence,
        properties: serde_json::json!({
            "evidence": {
                "rule": "db-verb-classify",
                "location": location,
                "evidence": format!("{} 调用 {}（{} 动词）", owner_fqn, method, kind_label(kind)),
            },
            "verb": method,
        }),
    });
    if added {
        ctx.propagation_seeds.push(crate::context::PropSeed {
            source: owner,
            target: table_id,
            kind: kind.0.clone(),
            confidence,
            sub,
            phase: Phase(Phase::RESOLVE.to_string()),
        });
    }
}

/// `WritesDb` / `ReadsDb` 的中文标签（仅用于边证据文案）。
fn kind_label(kind: &EdgeKind) -> &'static str {
    match kind.0.as_str() {
        EdgeKind::WRITES_DB => "写",
        EdgeKind::READS_DB => "读",
        _ => "",
    }
}

/// 容器解析：L1 字面 → L2 注册表 → L4 约定 → L6 与类全集求交。
fn resolve_container(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let raw = loc.raw.trim_start_matches('\\');

    // L1：字面 FQN
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Resolution::resolved(ResolveTier::Exact, id, format!("字面 FQN {raw}"));
    }
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(&loc.file), raw) {
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Resolution::resolved(ResolveTier::Exact, id, format!("短名解析 {raw} → {fqn}"));
        }
    }

    // L2：容器注册表（链式引用靠外层不动点收敛）
    if let Some(Value::Object(entry)) = ctx.ws.get_symbol("container_bindings", raw).cloned() {
        let target = entry
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim_start_matches('\\')
            .to_string();
        if !target.is_empty() {
            if let Some(id) = ctx
                .ws
                .find_by_name(&target)
                .or_else(|| {
                    ctx.ws
                        .resolve_name_in_file(Some(&loc.file), &target)
                        .and_then(|f| ctx.ws.find_by_name(&f))
                })
            {
                return Resolution::resolved(
                    ResolveTier::Registry,
                    id,
                    format!("provider.php 绑定 {raw} → {target}"),
                );
            }
            return Resolution::unknown(format!("间接绑定 {raw} → {target}"));
        }
        return Resolution::unknown(format!("闭包绑定 {raw}"));
    }

    // L4：约定（FQN 字符串）
    if raw.contains('\\') {
        if let Some(id) = ctx.ws.find_by_name(raw) {
            return Resolution::resolved(ResolveTier::Convention, id, format!("约定 {raw}"));
        }
    }

    // L6：与类短名全集求交
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(&loc.file), raw) {
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Resolution::resolved(ResolveTier::Intersection, id, format!("与类全集求交 {raw}"));
        }
    }

    Resolution::unknown(format!("容器无法解析 {raw}"))
}

/// 事件触发：查 P6 注册的 `by_alias(event_name, x)`。
fn resolve_event(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("event_name", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("事件别名 {}", loc.raw)),
        None => {
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "EventTriggerUnresolved",
                Severity::Warning,
                format!(
                    "event('{}') 未找到注册的事件节点 —— 若 event.php 里该键为空数组，\
                     监听器会被整体误判为死代码",
                    loc.raw
                ),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("事件 {} 未注册", loc.raw))
        }
    }
}

/// 运行时事件监听注册：`Event::listen('x', Listener::class)` / `Event::subscribe(Listener::class)`。
///
/// arg0（事件名）复用 `event_name` 别名索引解析到事件节点（与 `event('x')` 同一套），
/// 解析结果在 `apply_resolution` 里用于把 `HandledBy` 边从事件节点指向 arg1 监听器类。
fn resolve_event_listen(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("event_name", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("事件别名 {}", loc.raw)),
        None => {
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "EventListenUnresolved",
                Severity::Warning,
                format!(
                    "Event::listen('{}', …) 未找到对应事件节点 —— 该事件未在 event.php 注册，\
                     监听器仍可经 `listener` 标注识别，但无法精确挂到具体事件",
                    loc.raw
                ),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("事件 {} 未注册", loc.raw))
        }
    }
}

/// 把 `Foo::class` / `\App\X` 这类字面解析成图内的类节点。
fn resolve_class_node(ctx: &PipelineContext, raw: &str, file: Option<&str>) -> Option<NodeId> {
    let mut s = raw.trim_start_matches('\\').to_string();
    if let Some(stripped) = s.strip_suffix("::class") {
        s = stripped.to_string();
    }
    ctx.ws.find_by_name(&s).or_else(|| {
        ctx.ws
            .resolve_name_in_file(file, &s)
            .and_then(|f| ctx.ws.find_by_name(&f))
    })
}

/// Facade：查 P3 装载的 FacadeMap。
fn resolve_facade(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let receiver = loc.raw.trim_start_matches('\\');
    let Some(entry) = ctx.ws.get_symbol("facade_map", receiver).cloned() else {
        return Resolution::unknown(format!("非门面调用 {receiver}"));
    };
    let target = entry
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match ctx.ws.find_by_name(&target) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("门面 {receiver} → {target}")),
        None => {
            // vendor 被 P0 排除，目标不在图中是**预期**的，不能当错误报
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "AliasTargetMissing",
                Severity::Info,
                format!("门面目标 {target} 不在图内（ExcludedByIngest）"),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("门面目标 {target} 缺失"))
        }
    }
}

/// 获取器：`$order->status_text` → `getStatusTextAttr`（复合键，避免撞名）。
fn resolve_accessor(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("accessor", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("获取器 {}", loc.raw)),
        None => Resolution::unknown(format!("获取器 {} 未注册", loc.raw)),
    }
}

/// 路由 handler：`'Login/appleLogin'` → `app\api\controller\Login::appleLogin`。
fn resolve_handler(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match resolve_handler_target(ctx, &loc.raw, &loc.file, loc.sub, None) {
        Some((id, note)) => Resolution::resolved(
            ResolveTier::Convention,
            id,
            format!("handler {}（{}）", loc.raw, note),
        ),
        None => Resolution::unknown(format!("无法解析 handler {}", loc.raw)),
    }
}

/// 解析路由 handler，返回 (目标节点, 命中说明)。
///
/// 关键：**方法不存在时回退到类**。控制器方法常常继承自基类，
/// 要求 `Controller::method` 一定存在会让整条路由链断掉。
///
/// `method` 是**框架知识额外给出的入口方法**（如资源路由展开出的 `index` / `delete`，
/// 见 `PendingLink.method`）。handler 串里自带方法名时应以串里的为准（那是源码明文），
/// 串里只有类名（资源路由）时才用外部给的。
pub fn resolve_handler_target(
    ctx: &PipelineContext,
    raw: &str,
    file: &str,
    sub: Option<gt_domain::model::SubProjectId>,
    method: Option<&str>,
) -> Option<(NodeId, &'static str)> {
    // handler 的形态（分隔符 / 类名模板 / 应用段）全部由 FKB 声明 —— 内核不认识任何框架。
    let spec = sub
        .and_then(|s| ctx.handler_specs.get(&s.get()).cloned())
        .or_else(|| ctx.handler_spec_default.clone())
        .unwrap_or_default();
    // 命名空间 / 成员分隔符来自语言策略（PHP `\` + `::`，Java `.` + `.`）
    let policy = ctx.lang_policy_for_sub(sub).clone();

    let raw = raw.trim_start_matches('\\');
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Some((id, "完全限定名直接命中"));
    }

    // ① 按 FKB 声明的分隔符拆 controller / method。
    //    拆不出方法名时才用 FKB 额外给的入口方法（资源路由的 `expand_entry`）。
    let (controller_part, inline_method) = split_handler(raw, &spec.method_separators);
    let method = if inline_method.is_empty() {
        method.unwrap_or("").to_string()
    } else {
        inline_method
    };

    // ② controller 内部的层级字符 → 命名空间分隔符
    //    例：ThinkPHP 的 `v1.agent.AgentManage` → `v1\agent\AgentManage`
    let mut controller = controller_part.to_string();
    let ns_sep = policy.ns_separator.to_string();
    for sep in &spec.hierarchy_separators {
        if !sep.is_empty() {
            controller = controller.replace(sep.as_str(), &ns_sep);
        }
    }

    // ③ 候选类名 = 模板 × 应用段
    let app_seg = route_app_segment(file, &spec);
    let mut app_segs: Vec<String> = vec![app_seg];
    app_segs.extend(spec.app_segments.iter().cloned());
    let mut bases: Vec<String> = Vec::new();
    for tpl in &spec.class_templates {
        for seg in &app_segs {
            let b = tpl
                .replace("{app}", seg)
                .replace("{controller}", &controller);
            if !bases.contains(&b) {
                bases.push(b);
            }
        }
    }
    // 框架没声明模板时，controller 本身即候选（全限定名或短名，由 ⑤ 兜底）
    if bases.is_empty() {
        bases.push(controller.clone());
    }

    // ④ 方法优先，其次回退到类（控制器方法常继承自基类，要求方法存在会断链）
    if !method.is_empty() {
        for base in &bases {
            if let Some(id) = ctx.ws.find_by_name(&policy.join_member(base, &method)) {
                return Some((id, "方法精确命中"));
            }
        }
    }
    for base in &bases {
        if let Some(id) = ctx.ws.find_by_name(base) {
            return Some((id, "类命中（方法未在图内，可能继承自基类）"));
        }
    }

    // ⑤ 短名兜底：按路由文件里的 import 还原
    let short = controller
        .rsplit(|c: char| policy.ns_separators.contains(&c))
        .next()
        .unwrap_or(&controller);
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(file), short) {
        if !method.is_empty() {
            if let Some(id) = ctx.ws.find_by_name(&policy.join_member(&fqn, &method)) {
                return Some((id, "短名方法命中"));
            }
        }
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Some((id, "短名类命中"));
        }
    }
    None
}

/// 按 FKB 声明的**顺序**尝试分隔符，拆出 controller 与 method。
///
/// ThinkPHP 用 `/`、Laravel 与 Symfony 用 `@` 或 `::`、Rails 用 `#` —— 都由此支持。
fn split_handler(raw: &str, seps: &[String]) -> (String, String) {
    for s in seps {
        if s.is_empty() {
            continue;
        }
        if let Some((c, m)) = raw.split_once(s.as_str()) {
            return (c.to_string(), m.to_string());
        }
    }
    (raw.to_string(), String::new())
}

/// 从路由文件路径推断应用段：`app/api/route/pc.php` + 锚点 `route` → `api`。
fn route_app_segment(file: &str, spec: &HandlerSpec) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    if let Some(anchor) = &spec.app_anchor_dir {
        if let Some(pos) = parts.iter().position(|p| *p == anchor.as_str()) {
            if pos > 0 {
                return parts[pos - 1].to_string();
            }
        }
    }
    if !spec.app_fallback.is_empty() {
        return spec.app_fallback.clone();
    }
    parts.first().copied().unwrap_or("").to_string()
}

/// 把一次动态解析落成**可定位**的 `evidence` 对象（与 `engine.rs` 里 FKB 规则边同一形状）。
///
/// 关键在于 `location`：视图层靠 `properties.evidence.location` 取"这条边是在哪一行建立的"
/// （`view_service::inline_chain_locations` 补 `to_call_site`、`edge_evidence` 取证据位置，
/// 两处都读它）。此前这里只存 `res.evidence` 这**一个字符串**，形状对不上，于是
/// `Triggers` / `HandledBy` 这类语义边永远取不到位置 —— 抽屉里只剩"没有逐跳证据可查"，
/// 其实那一行（如 `event('X')` 的调用处）本来就在图里。
fn evidence_of(loc: &Locator, res: &Resolution) -> serde_json::Value {
    serde_json::json!({
        "rule": format!("{:?}", loc.strategy),
        "evidence": res.evidence.clone(),
        "location": format!("{}:{}", loc.file, loc.line),
    })
}

fn apply_resolution(ctx: &mut PipelineContext, loc: &Locator, res: &Resolution, phase: &Phase) {
    // 运行时事件监听注册：把 arg0 解析出的事件节点，用 `HandledBy` 边指向 arg1 监听器类。
    // 注意：这不是"触发"，故不走默认的 owner→target（Triggers）边。
    if loc.strategy == ResolveStrategy::EventListen {
        if let Some(event_id) = res.candidates.first() {
            match loc.consumer.as_deref() {
                Some(consumer) => {
                    if let Some(listener_id) = resolve_class_node(ctx, consumer, Some(&loc.file)) {
                        ctx.ws.add_edge(NewEdge {
                            project_id: ctx.project.id,
                            kind: EdgeKind(EdgeKind::HANDLED_BY.to_string()),
                            from_id: *event_id,
                            to_id: listener_id,
                            phase: phase.clone(),
                            confidence: res.confidence,
                            properties: serde_json::json!({
                                "tier": format!("{:?}", res.tier),
                                "evidence": evidence_of(loc, res),
                                "via": "Event::listen/subscribe 运行时注册",
                            }),
                        });
                    } else {
                        ctx.ws.diagnose(
                            phase,
                            "EventListenTargetMissing",
                            Severity::Warning,
                            format!("Event::listen 监听器 {} 不在图内", consumer),
                            Some(format!("{}:{}", loc.file, loc.line)),
                        );
                    }
                }
                None => {
                    ctx.ws.diagnose(
                        phase,
                        "EventListenNoConsumer",
                        Severity::Info,
                        format!("Event::listen('{}') 缺少监听器参数", loc.raw),
                        Some(format!("{}:{}", loc.file, loc.line)),
                    );
                }
            }
        }
        return;
    }
    let kind = match loc.strategy {
        ResolveStrategy::Event => EdgeKind(EdgeKind::TRIGGERS.to_string()),
        ResolveStrategy::Handler => EdgeKind(EdgeKind::HANDLED_BY.to_string()),
        // 实例方法调用：调用者 → 目标方法，是一条真正的「调用」边。
        ResolveStrategy::VariableType => EdgeKind(EdgeKind::CALLS.to_string()),
        _ => EdgeKind(EdgeKind::RESOLVES_TO.to_string()),
    };
    for target in &res.candidates {
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: kind.clone(),
            from_id: loc.owner,
            to_id: *target,
            phase: phase.clone(),
            confidence: res.confidence,
            properties: serde_json::json!({
                "tier": format!("{:?}", res.tier),
                "evidence": evidence_of(loc, res),
            }),
        });
    }
    if res.candidates.is_empty()
        && !matches!(
            loc.strategy,
            ResolveStrategy::Facade | ResolveStrategy::VariableType
        )
    {
        ctx.ws.diagnose(
            phase,
            "UnresolvedLink",
            Severity::Info,
            format!("{:?} 解析失败: {}", loc.strategy, res.evidence),
            Some(format!("{}:{}", loc.file, loc.line)),
        );
    }
}

/// 解析 P5 遗留的待定链接（路由 handler / 事件监听器等）。
fn resolve_pending_links(ctx: &mut PipelineContext, phase: &Phase) {
    let pending = std::mem::take(&mut ctx.ws.pending_links);
    for link in &pending {
        let target = match link.resolve {
            gt_domain::model::ResolveAs::HandlerPattern => {
                let (file, _) = link.file.split_once(':').unwrap_or((link.file.as_str(), "0"));
                resolve_handler_target(ctx, &link.raw, file, link.sub, link.method.as_deref())
                    .map(|(id, _)| id)
                    .or_else(|| {
                        ctx.ws
                            .resolve_name_in_file(Some(file), &link.raw)
                            .and_then(|f| ctx.ws.find_by_name(&f))
                    })
            }
            _ => {
                let (f, _) = link.file.split_once(':').unwrap_or((link.file.as_str(), "0"));
                ctx.ws.find_by_name(&link.raw).or_else(|| {
                    ctx.ws
                        .resolve_name_in_file(Some(f), &link.raw)
                        .and_then(|x| ctx.ws.find_by_name(&x))
                })
            }
        };
        match target {
            Some(t) => {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: link.kind.clone(),
                    from_id: link.from,
                    to_id: t,
                    phase: phase.clone(),
                    confidence: link.confidence,
                    properties: Value::Null,
                });
            }
            None => {
                ctx.ws.diagnose(
                    phase,
                    "UnresolvedLink",
                    Severity::Warning,
                    format!(
                        "{} 指向 {}，但图中找不到目标 —— 路由指向不存在的 handler 会导致运行时 500",
                        link.kind, link.raw
                    ),
                    Some(link.file.clone()),
                );
            }
        }
    }
}

fn fact_to_string(v: &FactValue) -> String {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// 通用调用解析：为可静态确定的调用建立 `Calls` 边（调用者方法 → 目标方法 / 函数）。
///
/// 为什么必须做：`ReadsConfig` / `ReadsCache` / `MapsTo` 等语义边挂在**被调方法的节点**上，
/// 若调用者与被调者之间没有边，整条「路由 → 缓存 / 配置 / 表」的链路在图上就是断的，
/// 视图只能靠 `Declares`/`HasCallSite` 之类的结构边绕路，最终把无关节点全拉进来。
///
/// 只解析能静态确定的调用，避免误连：
/// * `Class::method`（静态 / 门面调用）：receiver 是类名 → 解析类短名后拼 `Fqn::method`；
/// * 自由函数 `foo()`（排除 `new Foo`）→ 按名 / 短名解析；
/// * 变量接收者（`$this->x`、`$obj->y`）需要类型推断，**不解析**（宁可缺边，不可错边）。
fn resolve_calls(ctx: &mut PipelineContext, phase: &Phase) {
    let calls = ctx.ws.calls.clone();
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut added = 0usize;
    for call in &calls {
        if let Some(target) = resolve_call_target(ctx, call) {
            if target != call.owner && seen.insert((call.owner.get(), target.get())) {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: EdgeKind(EdgeKind::CALLS.to_string()),
                    from_id: call.owner,
                    to_id: target,
                    phase: phase.clone(),
                    confidence: 0.7,
                    properties: serde_json::json!({
                        "callee": call.callee,
                        // 精确记录这次调用对应的 CallSite 节点 id，视图层据此直接取到"调用处"，
                        // 无需再用被调名做启发式匹配。
                        "call_site": call.node.get(),
                    }),
                });
                added += 1;
            }
        }
        // 调用者 → **接收者类型类**：让类级语义边（`Dao → Model`、`Model → Table`）沿调用链浮现。
        // 例：`$this->dao->value()` 解析到的是继承来的 `BaseDao::value`（声明类没有 Model 链接），
        // 但接收者类型 `WechatUserDao` 才是 `setModel()` 指向 Model 的那一层。
        if let Some(recv) = call.receiver.as_deref() {
            if let Some(type_fqn) = receiver_type_fqn(ctx, &call.owner_fqn, recv) {
                let cid = ctx.ws.find_by_name(&type_fqn).or_else(|| {
                    ctx.ws
                        .resolve_name_at(call.owner, &type_fqn)
                        .and_then(|f| ctx.ws.find_by_name(&f))
                });
                if let Some(cid) = cid {
                    if cid != call.owner && seen.insert((call.owner.get(), cid.get())) {
                        ctx.ws.add_edge(NewEdge {
                            project_id: ctx.project.id,
                            kind: EdgeKind(EdgeKind::CALLS.to_string()),
                            from_id: call.owner,
                            to_id: cid,
                            phase: phase.clone(),
                            confidence: 0.7,
                            properties: serde_json::json!({
                                "via": "receiver_type",
                                "callee": call.callee,
                                "call_site": call.node.get(),
                            }),
                        });
                        added += 1;
                    }
                }
            }
        }
    }
    info!("P7 调用链解析：{} 条 Calls 边", added);
}

/// 解析调用接收者的类型 FQN：
/// * `$this->prop` → 属性类型（沿继承链回溯，`WechatServices` 找不到就看父类）；
/// * `$var` → 所在方法的参数类型。
fn receiver_type_fqn(ctx: &PipelineContext, owner_fqn: &str, recv: &str) -> Option<String> {
    // 裸 `$this`：类型就是定义该方法的类（`$this->save()` 即当前模型类）。
    // 此前漏掉这一支 —— `$this->X` 解析后 `receiver` 是裸 `$this`，走到下面 `$var` 分支
    // 因没有名为 `this` 的参数 / 局部变量而返回 `None`，于是 `resolve_variable_type`
    // 提前 `unknown` 退出，**模型类里的读写动词（`$this->save` / `$this->paginate`）永远
    // 落不出 WritesDb / ReadsDb**（yoshop 的 545 条「映射」基本都源于此）。
    if recv == "$this" {
        let class_fqn = owner_fqn.split("::").next().unwrap_or("");
        return if class_fqn.is_empty() {
            None
        } else {
            Some(class_fqn.to_string())
        };
    }
    // `static` / `self`：静态上下文里的「当前类」（如 `static::deleteAll()` /
    // `self::detail()`），类型同样是定义该方法的类。yoshop 大量写操作走 `static::*`，
    // 不处理这一支这些写动词永远落不出 `WritesDb`。
    if recv == "static" || recv == "self" {
        let class_fqn = owner_fqn.split("::").next().unwrap_or("");
        return if class_fqn.is_empty() {
            None
        } else {
            Some(class_fqn.to_string())
        };
    }
    // `parent::method()`：接收者类型是「定义该方法的类的直接父类」，让父类上的
    // `Model --MapsTo--> Table` 类级语义边能沿调用链浮现（与上面的 target 解析配套）。
    if recv == "parent" {
        let class_fqn = owner_fqn.split("::").next().unwrap_or("");
        if !class_fqn.is_empty() {
            if let Some(p) = ctx.ws.parents_of(class_fqn).into_iter().next() {
                return Some(p);
            }
        }
        return None;
    }
    // `(new static)->select()` / `(new Foo)->save()`：链式 builder 的接收者被 parser 记成
    // 整个 `new` 表达式。括号里的类就是实例类型 —— `static`/`self` 即「当前类」，
    // 普通类名则按已知类解析；都推不出时退回定义该方法的类。否则模型里的
    // `(new static)->where()->select()`（yoshop 的 `getList`/`getAll` 全是这种写法）
    // 永远推不出接收者类型，读 / 写动词落不出 `ReadsDb` / `WritesDb`，路由只能退回「映射到」。
    if let Some(inner) = recv.strip_prefix("(new ") {
        let class = inner.trim_end_matches(')').trim().trim_start_matches('\\');
        if class == "static" || class == "self" || class.is_empty() {
            let c = owner_fqn.split("::").next().unwrap_or("");
            return if c.is_empty() { None } else { Some(c.to_string()) };
        }
        if !class.is_empty() && ctx.ws.find_by_name(class).is_some() {
            return Some(class.to_string());
        }
        // 短名按所属文件 `use` 表还原（`use App\Model\X as XModel`）：
        // yoshop / CRMEB 大量 `XxxModel::getX()` / `(new XxxModel)` 用的就是导入别名，
        // 不还原就推不出接收者类型，读 / 写动词落不出动作边。
        if !class.is_empty() {
            if let Some(owner_id) = ctx.ws.find_by_name(owner_fqn) {
                if let Some(fqn) = file_import_of(ctx, owner_id, class) {
                    if ctx.ws.find_by_name(&fqn).is_some() {
                        return Some(fqn);
                    }
                }
                if let Some(fqn) = ctx.ws.resolve_name_at(owner_id, class) {
                    if ctx.ws.find_by_name(&fqn).is_some() {
                        return Some(fqn);
                    }
                }
            }
        }
        let c = owner_fqn.split("::").next().unwrap_or("");
        return if c.is_empty() { None } else { Some(c.to_string()) };
    }
    if let Some(prop) = recv.strip_prefix("$this->") {
        let class_fqn = owner_fqn.split("::").next().unwrap_or("");
        let mut cur = Some(class_fqn.to_string());
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(c) = cur {
            if !visited.insert(c.clone()) {
                break;
            }
            if let Some(ty) = ctx.ws.prop_type(&c, prop) {
                return Some(resolve_impl(ctx, &ty));
            }
            cur = ctx.ws.parents_of(&c).into_iter().next();
        }
        None
    } else if recv.starts_with('$') {
        let var = recv.trim_start_matches('$');
        // 先按**参数类型提示**（DI 注入），再按方法内的赋值推断（`$x = new Y()`）。
        let t = ctx
            .ws
            .param_type(owner_fqn, var)
            .or_else(|| ctx.ws.local_type(owner_fqn, var));
        if let Some(s) = t {
            return Some(resolve_impl(ctx, &s));
        }
        // `$model` 是 ThinkPHP / yoshop 里极其稳固的约定变量：模型方法里它指代「当前模型
        // 实例」（`$model->save()` / `$model->detail()` 即操作本表）。其类型提示 / 局部赋值
        // 常常缺失，类型推不出时退回「定义该方法的类」（即被映射的模型），让写 / 读动词
        // 能落成 `WritesDb` / `ReadsDb`——否则这类路由只能退回含糊的「映射到」。
        if var == "model" {
            let class_fqn = owner_fqn.split("::").next().unwrap_or("");
            if !class_fqn.is_empty() {
                return Some(class_fqn.to_string());
            }
        }
        None
    } else {
        // PHP 静态类调用 `ApiModel::detail()`：接收者就是该类本身（`ApiModel` 是类
        // 全名，作为方法调用的接收者）。先按「已知类」尝试，命中即把类型定为该类，
        // 于是 `ApiModel::save` / `ApiModel::detail` 这类写 / 读动词能落成 WritesDb /
        // ReadsDb（yoshop 的 `XxxModel::detail` / `XxxModel::save` 全走这种形态）。
        // 非类名的裸标识符（Java 字段 `mapper`）在这里 `find_by_name` 必然落空，
        // 自然落到下面的 Java 字段分支，不影响 Java 解析。
        if !recv.is_empty() && ctx.ws.find_by_name(recv).is_some() {
            return Some(recv.to_string());
        }
        // Java 字段（裸标识符）：从所属类（含父类）的字段类型推断
        // `service.mapper.findX()` 中 `mapper` 是 `@Autowired` 注入字段。
        let class_fqn = owner_class_of(owner_fqn);
        let mut cur = Some(class_fqn);
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(c) = cur {
            if !visited.insert(c.clone()) {
                break;
            }
            if let Some(ty) = ctx.ws.prop_type(&c, recv) {
                return Some(resolve_impl(ctx, &ty));
            }
            cur = ctx.ws.parents_of(&c).into_iter().next();
        }
        None
    }
}

/// 若 `type_fqn` 是接口且存在实现类，返回实现类的 FQN：Spring 注入的是实现类，
/// DB 调用也在实现类的方法体里；否则原样返回。
///
/// 不解析到实现类会断链：路由 → 服务接口（`@Autowired` 字段类型）→ 实现类方法
/// （真正 `mapper.xxx()` 的地方）→ Mapper → 表。
fn resolve_impl(ctx: &PipelineContext, type_fqn: &str) -> String {
    if let Some(id) = ctx.ws.find_by_name(type_fqn) {
        if ctx
            .ws
            .node(id)
            .map(|n| n.kind.is("Interface"))
            .unwrap_or(false)
        {
            if let Some(rec) = ctx
                .ws
                .inherits
                .iter()
                .find(|r| r.base == type_fqn && r.kind.as_str() == "Implements")
            {
                return rec.child_fqn.clone();
            }
        }
    }
    type_fqn.to_string()
}

/// 从「方法 FQN」取所属类 FQN：Java `pkg.Class.method` → `pkg.Class`，
/// PHP `Class::method` → `Class`。
fn owner_class_of(owner_fqn: &str) -> String {
    owner_fqn
        .rsplit_once('.')
        .or_else(|| owner_fqn.rsplit_once("::"))
        .map(|(c, _)| c.to_string())
        .unwrap_or_else(|| owner_fqn.to_string())
}

/// 用**调用方所在文件的 `use` 表**把短名还原成 FQN。
///
/// 返回 `Some(fqn)` 表示该文件确实导入了这个短名 —— 调用方必须**只**认这个 FQN，
/// 即使它不在图里（框架类）也不能再退回全局短名索引猜测。
/// 返回 `None` 表示该文件没有导入它（`new Foo`、全局命名空间写法等），交由后续兜底。
fn file_import_of(ctx: &PipelineContext, owner: NodeId, short: &str) -> Option<String> {
    let file_id = ctx.ws.node(owner)?.file_id?;
    ctx.ws
        .imports_of_file(file_id.get())
        .and_then(|m| m.get(&short.to_ascii_lowercase()))
        .cloned()
}

fn resolve_call_target(ctx: &PipelineContext, call: &CallRecord) -> Option<NodeId> {
    // 1) `Class::method`（静态 / 门面）：receiver 是类名（非变量）
    if let Some(recv) = &call.receiver {
        if !recv.starts_with('$') {
            let m = call.method.as_deref().unwrap_or("");
            if m.is_empty() {
                return None;
            }
            let recv = recv.trim_start_matches('\\');
            // `parent::method()`：调用定义在「当前类的直接父类」上的同名方法。
            // yoshop 模型大量 `override` 后 `parent::getList()` / `parent::getInfo()`，
            // 真正的读库动词落在父类方法体里 —— 不解析这一支，路由到表只能退回含糊的
            // 「映射」。先替换成父类 FQN 再按 `Class::method` 处理。
            let recv = if recv == "parent" {
                let owner_class = call.owner_fqn.split("::").next().unwrap_or("");
                match ctx.ws.parents_of(owner_class).into_iter().next() {
                    Some(p) => p,
                    None => return None,
                }
            } else if recv == "self" || recv == "static" {
                // `self::` / `static::` 指向「定义该方法的类」：`static::detail()` 即
                // 当前模型类的 `detail()`。不替换成 owner 类，这类读 / 写动词调用永远
                // 解析不到目标方法（`static::detail` 字面量查无此节点），动作边落不出。
                let owner_class = call.owner_fqn.split("::").next().unwrap_or("");
                if owner_class.is_empty() {
                    return None;
                }
                owner_class.to_string()
            } else {
                recv.to_string()
            };
            if let Some(id) = ctx.ws.find_by_name(&format!("{recv}::{m}")) {
                return Some(id);
            }
            // 把接收者类名还原成 FQN：先按本文件 `use` 表（PHP 真实解析规则），
            // 再按命名空间解析。
            //
            // 关键：若文件导入的这个类**不在图里**（框架类，如 `think\facade\Cache`，
            // vendor 已被 P0 排除），就必须**放弃解析**，绝不能退回全局短名索引去猜
            // 一个同名的项目类。`Cache` 会被猜成 `app\model\other\Cache`（一个 Model），
            // 于是每次 `Cache::xxx()` 都凭空多出一条 `Calls` 边，并把
            // `Model --MapsTo--> Table(cache)` 这类类级语义边拖到路由上。
            let recv_fqn = if let Some(fqn) = file_import_of(ctx, call.owner, &recv) {
                fqn
            } else if let Some(fqn) = ctx.ws.resolve_name_at(call.owner, &recv) {
                fqn
            } else {
                recv.clone()
            };
            // 在 `recv_fqn` 及其祖先类里找方法：方法常**只声明在基类**（如 yoshop 的
            // `app\store\model\GoodsSpecRel` extends `app\common\model\GoodsSpecRel`，
            // `getSpecList` 只在基类声明）。不沿继承链回溯，子类 `XxxModel::getSpecList`
            // 就只能落到「类」上，读 / 写动词永远落不出动作边。框架父类（vendor）不在图里，
            // `find_by_name` 自然落空，不会误连。
            {
                let mut cur = Some(recv_fqn.clone());
                let mut seen: HashSet<String> = HashSet::new();
                let mut depth = 0usize;
                while let Some(c) = cur {
                    if depth > 20 || !seen.insert(c.clone()) {
                        break;
                    }
                    if let Some(id) = ctx.ws.find_by_name(&format!("{c}::{m}")) {
                        return Some(id);
                    }
                    cur = ctx.ws.parents_of(&c).into_iter().next();
                    depth += 1;
                }
            }
            // 方法不在图内（框架方法如 `User::where`）→ 连到**类本身**，
            // 从而让 Model --MapsTo--> Table 这类"类级语义边"能沿调用链浮现。
            if let Some(id) = ctx.ws.find_by_name(&recv_fqn) {
                return Some(id);
            }
        }
        return None;
    }
    // 2) 自由函数 `foo()`（排除 `new Foo`）
    //
    // **只接受解析到 `Function` 节点的结果**：`find_by_name` 是按 FQN 查的，而全局
    // helper 的名字会与命名空间 / 类撞名 —— `app()` 命中 Namespace `app`、
    // `config()` 命中同名类方法。此前不校验种类，凭空连出大量错误 Calls 边。
    // 宁可缺边，不可错边。
    let callee = call.callee.trim();
    if callee.is_empty() || callee.starts_with("new ") {
        return None;
    }
    if let Some(id) = ctx.ws.find_by_name(callee) {
        if is_function_node(ctx, id) {
            return Some(id);
        }
    }
    let id = ctx
        .ws
        .resolve_name_at(call.owner, callee)
        .and_then(|f| ctx.ws.find_by_name(&f))?;
    if is_function_node(ctx, id) {
        return Some(id);
    }
    None
}

/// 是否为自由函数节点（决定自由函数调用能否连到它）。
fn is_function_node(ctx: &PipelineContext, id: NodeId) -> bool {
    ctx.ws
        .node(id)
        .map(|n| n.kind.as_str() == NodeKind::FUNCTION)
        .unwrap_or(false)
}
