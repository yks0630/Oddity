// 多层弹窗按打开顺序叠放: 后打开的始终在上层.
// 层号只用于比较先后, 关闭后不在剩余弹窗之间重新分配, 避免动画期间层序跳动;
// 但下一个层号要收缩到当前最高层, 避免父弹窗常开时反复开关子弹窗把 z-index 越抬越高.
const openLayers = new Map<number, number>();
let nextId = 1;
let nextLayer = 0;

export interface ModalLayer {
  id: number;
  layer: number;
}

export function pushModalLayer(): ModalLayer {
  const id = nextId++;
  nextLayer += 1;
  openLayers.set(id, nextLayer);
  return { id, layer: nextLayer };
}

export function popModalLayer(id: number) {
  if (!openLayers.delete(id)) return;
  nextLayer = topModalLayer();
}

export function topModalLayer() {
  let top = 0;
  for (const layer of openLayers.values()) if (layer > top) top = layer;
  return top;
}

export function isTopModalLayer(layer: number) {
  return layer > 0 && layer >= topModalLayer();
}
