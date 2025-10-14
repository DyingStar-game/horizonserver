import { Faker } from "@faker-js/faker/.";
import { BuilderBase } from "../BuilderBase";
import {
  gorcEventCh0WsSchema,
  GorcEventCh0WsType,
} from "../../model/gorc/gorcEvent.ws.model";
import {
  Coordinate3dType,
  GorcEventEnum,
  GorcTypeEnum,
} from "../../model/gorc/gorcBase.ws.model";
import { aCoordinate3d } from "../../builders";

export class MovePlayerEventWsBuilder extends BuilderBase<
  typeof gorcEventCh0WsSchema
> {
  constructor(faker: Faker) {
    const playerId = faker.string.uuid();

    super(gorcEventCh0WsSchema, {
      channel: 0,
      event: faker.helpers.enumValue(GorcEventEnum),
      object_id: faker.string.uuid(),
      player_id: playerId,
      type: GorcTypeEnum.EVENT,
      timestamp: faker.date.recent().getTime(),
      data: {
        client_timestamp: faker.date.recent().toISOString(),
        movement_state: faker.number.int({ min: 1, max: 10 }),
        player_id: playerId,
        new_position: aCoordinate3d(faker).build(),
        velocity: aCoordinate3d(faker).build(),
      },
    } as GorcEventCh0WsType);
  }

  withObjectId(objectId: string): this {
    this.data.object_id = objectId;
    return this;
  }

  withPlayerId(playerId: string): this {
    this.data.player_id = playerId;
    this.data.data = {
      ...this.data.data,
      player_id: playerId,
    } as GorcEventCh0WsType["data"];
    return this;
  }

  withNewPosition(newPos: Coordinate3dType): this {
    this.data.data = {
      ...this.data.data,
      new_position: newPos,
    } as GorcEventCh0WsType["data"];
    return this;
  }

  withVelocity(velocity: Coordinate3dType): this {
    this.data.data = {
      ...this.data.data,
      velocity: velocity,
    } as GorcEventCh0WsType["data"];
    return this;
  }
}
