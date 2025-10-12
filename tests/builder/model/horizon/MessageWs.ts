import { MessageDataInterface } from "./data/MessageDataInterface";

export enum EventWs {
  INIT = "init",
}

export enum NamespaceWs {
  PLAYER = "player",
}

export class MessageWs<T extends MessageDataInterface> {
  public namespace: NamespaceWs;
  public event: EventWs;
  public data: T;

  constructor(namespace: NamespaceWs, event: EventWs, data: T) {
    this.namespace = namespace;
    this.event = event;
    this.data = data;
  }
}
