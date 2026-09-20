/// The names of the public interface resolve at the crate root (the `use` below is
/// the list). Written as a unit test, hence `crate::` instead of `vauban_tds::`.
#[test]
fn interface_names_resolve_at_root() {
    use crate::{
        ClientMessage, ColumnFlags, ColumnMeta, DoneStatus, EncryptPolicy, EnvChange, FeatureAck,
        FeatureExt, Login7, PacketType, ResetConnection, Rpc, RpcParam, RpcProc, SqlBatch,
        TdsError, TdsReader, TdsStream, TdsWriter, TmRequest, Token,
    };

    fn exists<T>() {}
    exists::<EncryptPolicy>();
    exists::<ClientMessage>();
    exists::<Login7>();
    exists::<SqlBatch>();
    exists::<Rpc>();
    exists::<RpcProc>();
    exists::<RpcParam>();
    exists::<Token>();
    exists::<ColumnMeta>();
    exists::<ColumnFlags>();
    exists::<EnvChange>();
    exists::<DoneStatus>();
    exists::<FeatureExt>();
    exists::<FeatureAck>();
    exists::<TmRequest>();
    exists::<TdsError>();
    exists::<TdsStream>();
    exists::<TdsReader>();
    exists::<TdsWriter>();
    exists::<PacketType>();
    exists::<ResetConnection>();
}
