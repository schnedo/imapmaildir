use crate::imap::transport::{ResponseData, connection};

/// Handles untagged responses while waiting for a command's completion
pub trait ResponseHandler {
    /// Error of handling a response
    type Error: From<connection::Error>;

    /// Handles `response`, or returns it back if it is not of interest
    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>>;

    /// Like [`Self::handle`], but logs responses that were not handled
    fn handle_or_log(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        async {
            if let Some(response) = self.handle(response).await? {
                log::trace!("unhandled untagged response {:?}", response.parsed());
            }

            Ok(())
        }
    }
}

/// Tries `A` first and passes unhandled responses on to `B`
pub struct Chain<A, B>(pub A, pub B);

impl<A: ResponseHandler<Error = B::Error>, B: ResponseHandler> ResponseHandler for Chain<A, B> {
    type Error = B::Error;

    async fn handle(
        &mut self,
        response: ResponseData,
    ) -> Result<Option<ResponseData>, Self::Error> {
        if let Some(response) = self.0.handle(response).await? {
            self.1.handle(response).await
        } else {
            Ok(None)
        }
    }
}

impl<H: ResponseHandler> ResponseHandler for &mut H {
    type Error = H::Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        (**self).handle(response)
    }
}
