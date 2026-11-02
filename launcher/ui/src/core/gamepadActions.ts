import { reactive } from "vue";

export interface KeyboardActions {
  cycle: () => void;
  commit: () => void;
  backspace: () => void;
  clear: () => void;
}

export interface FilePickerActions {
  primary: () => void;
  parent: () => void;
  reset: () => void;
}

// 屏幕键盘和文件选择框把动作注册进来, 由手柄分发层按当前界面调用.
export const gamepadActions = reactive({
  keyboard: null as KeyboardActions | null,
  picker: null as FilePickerActions | null,
});
