pub trait DarwinTranslation<HostType> {
    fn to_host(&self) -> HostType;

    fn from_host(host: &HostType) -> Self;
}
