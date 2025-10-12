import { Faker } from "@faker-js/faker/.";
import { EventWs, MessageWs, NamespaceWs } from "../model/horizon/MessageWs";
import { MessageDataInterface } from "../model/horizon/data/MessageDataInterface";

export class MessageWsBuilder<T extends MessageDataInterface> {
  private namespace: NamespaceWs;
  private event: EventWs;
  private data: T;

  constructor(faker: Faker) {
    this.namespace = faker.helpers.enumValue(NamespaceWs);
    this.event = faker.helpers.enumValue(EventWs);
    this.data = {} as T;
  }

  public build(): MessageWs<T> {
    return new MessageWs(this.namespace, this.event, this.data);
  }

  public withNamespace(namespace: NamespaceWs): this {
    this.namespace = namespace;
    return this;
  }

  public withEvent(event: EventWs): this {
    this.event = event;
    return this;
  }

  public withData(data: T): this {
    this.data = data;
    return this;
  }
}
