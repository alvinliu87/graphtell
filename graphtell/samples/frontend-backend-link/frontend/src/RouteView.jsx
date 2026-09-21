// 页面跳转示例：验证前端 `Page` 语义节点（来自 pages.json）与 `NavigatesTo`
// 语义边（来自 uni.navigateTo）能像后端 Route 一样建进图。
export function goDetail(id) {
  uni.navigateTo({ url: '/pages/detail/detail?id=' + id });
}

export function goList() {
  uni.navigateTo({ url: 'pagesA/list/list' });
}

// 事件总线示例：验证前端 EventBus 语义节点（emi / listen 汇聚到同一事件）。
export function emitRefresh() {
  uni.$emit('listRefresh');
}

export function onRefresh(cb) {
  uni.$on('listRefresh', cb);
}
