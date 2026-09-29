// uni-app 项目里 HTTP 客户端的典型封装：真正发请求只有这一处，
// 且 URL 是 `BASE + '/api' + url` 的动态拼串 —— 这一段解析器抓不到
// （会给 Unknown，避免合成 `GET /<dynamic-url>` 垃圾契约），前端真正
// 可被静态定位的调用点是调用方 `request.get(...)` 那层。
const BASE = 'https://example.com';

const request = {
  get(url, data, options) {
    return uni.request({ url: BASE + '/api' + url, method: 'GET', data, ...options });
  },
  post(url, data, options) {
    return uni.request({ url: BASE + '/api' + url, method: 'POST', data, ...options });
  },
};

export default request;
