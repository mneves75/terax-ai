type KeyboardEventLike = {
  keyCode?: number;
  which?: number;
  nativeEvent?: {
    isComposing?: boolean;
  };
};

export function isImeComposing(event: KeyboardEventLike): boolean {
  return (
    event.nativeEvent?.isComposing === true ||
    event.keyCode === 229 ||
    event.which === 229
  );
}
