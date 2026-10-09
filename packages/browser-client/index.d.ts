export interface Member { player_id: string; username: string; ready: boolean; peer_id?: string | null }
export interface Lobby { id: string; game_id: string; compatibility_id: string; capacity: number; status: 'Waiting'|'Connecting'|'InProgress'; is_owner: boolean; players: Member[]; session_id: string|null; revision: number }
export interface SessionContext { schema_version: 1; session_id: string; game_id: string; compatibility_id: string; capacity: number; participants: Member[]; local_player_id: string; signaling_url: string; ice_servers: RTCIceServer[]; expires_at: number }
export class AllumetteError extends Error { status: number; code: string }
export class AllumetteBrowserClient {
 constructor(baseUrl: string, options?: {fetch?: typeof fetch}); token: string|null;
 loginGuest(username: string): Promise<{player_id:string;username:string}>;
 listLobbies(signal?: AbortSignal): Promise<Lobby[]>;
 createLobby(gameId:string,compatibilityId:string,capacity?:number):Promise<Lobby>;
 createInvitation(id:string):Promise<{invitation_token:string;expires_at:number}>;
 joinLobby(id:string,invitationToken:string,compatibilityId:string):Promise<Lobby>;
 setReady(id:string,ready:boolean,compatibilityId:string):Promise<Lobby>;
 startSession(id:string):Promise<Lobby>;
 getSessionContext(id:string,signal?:AbortSignal):Promise<SessionContext>;
 endSession(id:string):Promise<null>; leaveLobby(id:string):Promise<null>; logout():void;
}
