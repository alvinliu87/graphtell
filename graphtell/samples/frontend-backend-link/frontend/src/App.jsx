import { deleteItem } from './api';

// 组件方法：跨文件调用前端的 api 函数，形成前端调用链
// App.onDelete → api.deleteItem →(CallsHttp)→ HttpContract。
export function onDelete() {
  deleteItem();
}
