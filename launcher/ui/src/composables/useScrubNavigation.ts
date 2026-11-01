import { nextTick, onBeforeUnmount, onMounted, ref, type Ref } from "vue";

type ScrubAxis = "x" | "y" | "auto";

// 分类条与顶部导航共用同一套按住拖动逻辑, 避免各处自己维护阈值与取消规则。
export function useScrubNavigation(options: {
  root: Ref<HTMLElement | null>;
  item: string;
  value: (element: HTMLElement) => string | null | undefined;
  active: () => string | null | undefined;
  select: (value: string) => void;
  axis?: ScrubAxis;
  corridor?: number;
  enabled?: () => boolean;
  focusOnFinish?: boolean;
  scrollIntoView?: boolean;
  scrubbing?: Ref<boolean>;
}) {
  const scrubbing = options.scrubbing ?? ref(false);
  const corridor = options.corridor ?? 24;
  let gesture:
    | {
        id: number;
        x: number;
        y: number;
        axis: "x" | "y";
        previous: string | null | undefined;
      }
    | undefined;
  let suppressClick = false;
  let finishing = 0;
  function items() {
    return [
      ...(options.root.value?.querySelectorAll<HTMLElement>(options.item) ??
        []),
    ];
  }
  function activeItem() {
    const active = options.active();
    return items().find((element) => options.value(element) === active) ?? null;
  }
  function detectAxis() {
    if (options.axis && options.axis !== "auto") return options.axis;
    const list = items();
    if (list.length < 2) return "x";
    const first = list[0].getBoundingClientRect();
    const last = list.at(-1)!.getBoundingClientRect();
    return Math.abs(last.top - first.top) > Math.abs(last.left - first.left)
      ? "y"
      : "x";
  }
  function inCorridor(event: PointerEvent) {
    const box = options.root.value?.getBoundingClientRect();
    if (!box || !gesture) return false;
    return gesture.axis === "x"
      ? event.clientY >= box.top - corridor &&
          event.clientY <= box.bottom + corridor
      : event.clientX >= box.left - corridor &&
          event.clientX <= box.right + corridor;
  }
  function selectAt(event: PointerEvent) {
    if (!gesture) return;
    const axis = gesture.axis;
    const pointer = axis === "x" ? event.clientX : event.clientY;
    const targets = items().map((element) => {
      const box = element.getBoundingClientRect();
      return {
        element,
        center:
          axis === "x" ? box.left + box.width / 2 : box.top + box.height / 2,
      };
    });
    if (!targets.length) return;
    const nearest = targets.reduce((best, entry) =>
      Math.abs(entry.center - pointer) < Math.abs(best.center - pointer)
        ? entry
        : best,
    ).element;
    const active = activeItem();
    // 选中层切换会带动布局微移, 在相邻两项中间留一段死区防止来回跳。
    if (active && active !== nearest) {
      const activeBox = active.getBoundingClientRect();
      const nearestBox = nearest.getBoundingClientRect();
      const middle = (box: DOMRect) =>
        axis === "x" ? box.left + box.width / 2 : box.top + box.height / 2;
      if (Math.abs(pointer - (middle(activeBox) + middle(nearestBox)) / 2) < 3)
        return;
    }
    const value = options.value(nearest);
    if (value != null && value !== options.active()) {
      options.select(value);
      if (options.scrollIntoView)
        nearest.scrollIntoView({
          block: "nearest",
          inline: "nearest",
          behavior: "instant",
        });
    }
  }
  function down(event: PointerEvent) {
    if (!event.isPrimary || event.button !== 0 || gesture) return;
    suppressClick = false;
    if (options.enabled && !options.enabled()) return;
    const root = options.root.value;
    if (!root || !(event.target instanceof Element)) return;
    const target = event.target.closest<HTMLElement>(options.item);
    if (!target || !root.contains(target)) return;
    finishing++;
    scrubbing.value = false;
    gesture = {
      id: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      axis: detectAxis(),
      previous: options.active(),
    };
  }
  function move(event: PointerEvent) {
    if (!gesture || event.pointerId !== gesture.id) return;
    const dx = event.clientX - gesture.x;
    const dy = event.clientY - gesture.y;
    if (!scrubbing.value) {
      const along = gesture.axis === "x" ? dx : dy;
      const across = gesture.axis === "x" ? dy : dx;
      // 先判断垂直方向, 避免页面纵向滚动或划选被误当成切类。
      if (Math.abs(across) > 8 && Math.abs(across) > Math.abs(along)) {
        void finish(true);
        return;
      }
      if (Math.abs(along) < 6) return;
      scrubbing.value = true;
      suppressClick = true;
      options.root.value?.setPointerCapture(event.pointerId);
    }
    if (inCorridor(event)) selectAt(event);
  }
  async function finish(cancelled: boolean) {
    if (!gesture) return;
    const token = ++finishing;
    const previous = gesture;
    const wasScrubbing = scrubbing.value;
    gesture = undefined;
    if (cancelled) {
      suppressClick = true;
      if (
        wasScrubbing &&
        previous.previous != null &&
        options.active() !== previous.previous
      )
        options.select(previous.previous);
    }
    if (options.root.value?.hasPointerCapture(previous.id))
      options.root.value.releasePointerCapture(previous.id);
    await nextTick();
    if (token !== finishing) return;
    scrubbing.value = false;
    if (wasScrubbing) {
      const active = activeItem();
      if (options.scrollIntoView && active)
        active.scrollIntoView({
          block: "nearest",
          inline: "nearest",
          behavior: "instant",
        });
      if (options.focusOnFinish !== false)
        active?.focus({ preventScroll: true });
    }
  }
  function up(event: PointerEvent) {
    if (!gesture || event.pointerId !== gesture.id) return;
    const inside = inCorridor(event);
    if (scrubbing.value && inside) selectAt(event);
    void finish(!inside);
  }
  function cancel(event?: PointerEvent) {
    if (gesture && (!event || event.pointerId === gesture.id))
      void finish(true);
  }
  function lost(event: PointerEvent) {
    if (event.target === options.root.value) cancel(event);
  }
  function key(event: KeyboardEvent) {
    if (event.key !== "Escape" || !gesture) return;
    event.preventDefault();
    event.stopPropagation();
    cancel();
  }
  function blurred() {
    cancel();
  }
  // 拖动结束后浏览器仍会补发 click, 这里只拦截指针产生的 click, 键盘激活不受影响。
  function suppressed(event: MouseEvent) {
    if (!suppressClick || event.detail === 0) return false;
    event.preventDefault();
    return true;
  }
  onMounted(() => {
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", cancel);
    window.addEventListener("blur", blurred);
    window.addEventListener("keydown", key, true);
  });
  onBeforeUnmount(() => {
    finishing++;
    gesture = undefined;
    scrubbing.value = false;
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
    window.removeEventListener("pointercancel", cancel);
    window.removeEventListener("blur", blurred);
    window.removeEventListener("keydown", key, true);
  });
  return { scrubbing, down, lost, cancel, suppressed };
}
