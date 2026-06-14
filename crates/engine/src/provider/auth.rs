pub(crate) struct LoginCallbacks<'a> {
    pub(crate) on_prompt: &'a (dyn Fn(&str, &str) + Send + Sync),
    pub(crate) on_progress: &'a (dyn Fn(&str) + Send + Sync),
}
