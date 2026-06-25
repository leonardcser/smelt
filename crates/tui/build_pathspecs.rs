pub(crate) fn git_pathspecs<'a>(head_ref: Option<&'a str>) -> Vec<&'a str> {
    let mut pathspecs = vec!["HEAD"];
    if let Some(head_ref) = head_ref {
        if !head_ref.is_empty() && head_ref != "HEAD" {
            pathspecs.push(head_ref);
        }
    }
    pathspecs.extend(["refs/tags", "packed-refs"]);
    pathspecs
}
