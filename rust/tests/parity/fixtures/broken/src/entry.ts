import { greeting } from "./héllo-世界";
import { bigTotal } from "./big";

export function run(name: string): string {
  return `${greeting(name)} ${bigTotal()}`;
}
