import { Faker } from "@faker-js/faker/.";
import {
  playerLogingWsSchema,
  PlayerLogingWsType,
} from "../model/playerLogin.ws.model";
import { BuilderBase } from "./BuilderBase";
import { EventWs, NamespaceWs } from "../model/message.ws.model";

export class PlayerLoginWsBuilder extends BuilderBase<
  typeof playerLogingWsSchema
> {
  constructor(faker: Faker) {
    super(playerLogingWsSchema, {
      namespace: NamespaceWs.PLAYER,
      event: EventWs.INIT,
      data: {
        login: faker.internet.username(),
        password: faker.internet.password(),
      },
    });
  }

  withNamespace(namespace: NamespaceWs): this {
    this.data.namespace = namespace;
    return this;
  }

  withEvent(event: EventWs): this {
    this.data.event = event;
    return this;
  }

  withLogin(login: string): this {
    this.data.data = { ...this.data.data, login } as PlayerLogingWsType["data"];
    return this;
  }

  withPassword(password: string): this {
    this.data.data = {
      ...this.data.data,
      password,
    } as PlayerLogingWsType["data"];
    return this;
  }
}
