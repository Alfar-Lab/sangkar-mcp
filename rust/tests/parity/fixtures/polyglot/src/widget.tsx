import { alpha } from "./chain";

export class Frame {
  render(): string {
    return "frame";
  }
}

export class BorderedFrame extends Frame {
  render(): string {
    return `[${super.render()}]`;
  }
}

export function Widget({ count }: { count: number }) {
  return <div>{new BorderedFrame().render() + alpha(count)}</div>;
}
